//! The badge: upstream's "Claude is using your computer" pill (Claude
//! Desktop 5.1231 cu-glow page, `#badge`).
//!
//! Timing is a pure function of the milliseconds since the flash started
//! ([`pose`], [`dot`]); layout and painting need only a font, which is one
//! embedded subset of Noto Sans Medium (SIL OFL 1.1, see
//! `assets/OFL.txt`) rasterized by `ab_glyph` (pure Rust).
//!
//! Geometry and look follow the page's CSS: a flex row with 8 px gap,
//! padding 8 px 16 px, 1 px border `rgba(217, 119, 87, 0.2)`, background
//! `rgba(255, 247, 242, 0.95)`, radius 999 px, 13 px text in clay, and an
//! 8 px clay dot that pulses (`dot-pulse 1.8s ease-in-out infinite`). The
//! `badge-center-to-corner 6s cubic-bezier(0.4, 0, 0.2, 1) forwards`
//! animation flashes it in at the centre of the output (scale 1.4), holds it
//! until 88 % and slides it to the top-right corner (`top: 40px`,
//! `left: calc(100% - 280px)`, scale 0.8) while fading it out. Under
//! `prefers-reduced-motion` the page never shows it.

use std::sync::OnceLock;

use ab_glyph::{Font, FontRef, Glyph, GlyphId, PxScale, point};
use tiny_skia::{FillRule, Paint, PathBuilder, PixmapMut, Transform};

use crate::anim::{CubicBezier, EASE_IN_OUT};
use crate::render::{CLAY, apply_opacity, bgra, clear};

/// The built-in text of `show {"badge": true}`.
pub const DEFAULT_TEXT: &str = "Claude is using your computer";

/// Longest badge text drawn, in characters (the rest is cut).
pub const MAX_TEXT_CHARS: usize = 120;

/// `animation: badge-center-to-corner 6s`.
pub const FLASH_MS: f64 = 6000.0;
/// Keyframe 5 %: the flash-in is over (300 ms).
pub const IN_MS: f64 = 0.05 * FLASH_MS;
/// Keyframe 88 %: the slide to the corner starts.
pub const HOLD_END_MS: f64 = 0.88 * FLASH_MS;
/// The per-keyframe easing, `cubic-bezier(0.4, 0, 0.2, 1)`.
pub const FLASH_EASE: CubicBezier = CubicBezier::new(0.4, 0.0, 0.2, 1.0);
/// `dot-pulse 1.8s ease-in-out infinite`.
pub const DOT_PULSE_MS: f64 = 1800.0;

/// `scale(1.4)` at the centre, `scale(0.8)` at the corner.
pub const SCALE_CENTER: f64 = 1.4;
pub const SCALE_CORNER: f64 = 0.8;
/// The corner keyframe: `top: 40px; left: calc(100% - 280px)`.
const CORNER_TOP: f64 = 40.0;
const CORNER_LEFT_FROM_RIGHT: f64 = 280.0;
/// A pill that would leave the output keeps this margin to its edge.
const EDGE_MARGIN: f64 = 8.0;

const FONT_PX: f32 = 13.0;
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 8.0;
const GAP: f32 = 8.0;
const BORDER: f32 = 1.0;
const DOT_PX: f32 = 8.0;
/// Transparent margin around the pill inside its surface (logical px).
const BOX_MARGIN: f64 = 2.0;

static FONT_DATA: &[u8] = include_bytes!("../assets/NotoSans-Medium-subset.ttf");

fn font() -> &'static FontRef<'static> {
    static FONT: OnceLock<FontRef<'static>> = OnceLock::new();
    FONT.get_or_init(|| FontRef::try_from_slice(FONT_DATA).expect("embedded font parses"))
}

/// Strip what a one-line label cannot show (control characters become
/// spaces), cut to [`MAX_TEXT_CHARS`], trim. `None` when nothing is left.
pub fn clean_text(s: &str) -> Option<String> {
    let t: String = s
        .chars()
        .take(MAX_TEXT_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The pill's size and text layout in logical (CSS) pixels at scale 1.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// Pill size including border and padding.
    pub w: f64,
    pub h: f64,
    glyphs: Vec<(GlyphId, f32)>,
    /// Baseline offset below the top of the text line.
    ascent: f32,
}

impl Layout {
    pub fn new(text: &str) -> Self {
        let f = font();
        let upm = f.units_per_em().unwrap_or(1000.0);
        let k = FONT_PX / upm;
        let mut x = 0.0f32;
        let mut glyphs = Vec::new();
        for c in text.chars() {
            let id = f.glyph_id(c);
            glyphs.push((id, x));
            x += f.h_advance_unscaled(id) * k;
        }
        // `line-height: normal`: rounded ascent and descent, like Blink
        let ascent = (f.ascent_unscaled() * k).round();
        let descent = (-f.descent_unscaled() * k).round();
        let line = (ascent + descent).max(DOT_PX);
        Layout {
            w: f64::from(2.0 * BORDER + 2.0 * PAD_X + DOT_PX + GAP + x),
            h: f64::from(2.0 * BORDER + 2.0 * PAD_Y + line),
            glyphs,
            ascent,
        }
    }

    /// The pill drawn at the flash's largest scale fits a box of this
    /// logical size (with a small transparent margin); the surface keeps
    /// that size for the whole flash, so its buffers are reused.
    pub fn box_size(&self) -> (i32, i32) {
        (
            (self.w * SCALE_CENTER + 2.0 * BOX_MARGIN).ceil() as i32,
            (self.h * SCALE_CENTER + 2.0 * BOX_MARGIN).ceil() as i32,
        )
    }
}

/// Where the pill is and how it looks, in output-local logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub opacity: f64,
    /// Centre of the pill.
    pub cx: f64,
    pub cy: f64,
    pub scale: f64,
}

fn lerp(a: f64, b: f64, u: f64) -> f64 {
    a + (b - a) * u
}

/// The flash `ms` after its start on an output of `out` logical size, for a
/// pill of `w` x `h` (unscaled); `None` once it is over (the animation ends
/// at opacity 0 and stays there).
pub fn pose(ms: f64, out: (f64, f64), w: f64, h: f64) -> Option<Pose> {
    if ms >= FLASH_MS {
        return None;
    }
    let ms = ms.max(0.0);
    let (ow, oh) = out;
    let (opacity, left, top, tx, ty, scale) = if ms < IN_MS {
        let u = FLASH_EASE.at(ms / IN_MS);
        (u, ow / 2.0, oh / 2.0, -w / 2.0, -h / 2.0, SCALE_CENTER)
    } else if ms < HOLD_END_MS {
        (1.0, ow / 2.0, oh / 2.0, -w / 2.0, -h / 2.0, SCALE_CENTER)
    } else {
        let u = FLASH_EASE.at((ms - HOLD_END_MS) / (FLASH_MS - HOLD_END_MS));
        (
            1.0 - u,
            lerp(ow / 2.0, ow - CORNER_LEFT_FROM_RIGHT, u),
            lerp(oh / 2.0, CORNER_TOP, u),
            lerp(-w / 2.0, 0.0, u),
            lerp(-h / 2.0, 0.0, u),
            lerp(SCALE_CENTER, SCALE_CORNER, u),
        )
    };
    // `transform-origin` is the box centre, so scaling never moves it
    let mut cx = left + tx + w / 2.0;
    let cy = top + ty + h / 2.0;
    // a long (localized) text must not leave the output at the corner
    cx = cx.min(ow - EDGE_MARGIN - scale * w / 2.0);
    Some(Pose {
        opacity,
        cx,
        cy,
        scale,
    })
}

/// The dot's opacity and scale `ms` into the flash.
pub fn dot(ms: f64) -> (f64, f64) {
    let p = (ms.max(0.0) % DOT_PULSE_MS) / DOT_PULSE_MS;
    let k = if p < 0.5 {
        EASE_IN_OUT.at(p * 2.0)
    } else {
        1.0 - EASE_IN_OUT.at((p - 0.5) * 2.0)
    };
    (1.0 - 0.5 * k, 1.0 - 0.3 * k)
}

/// Whether frame-paced updates are needed `ms` into the flash (the
/// flash-in and the slide); the hold only needs the dot's timer.
pub fn moving(ms: f64) -> bool {
    ms < IN_MS || (HOLD_END_MS..FLASH_MS).contains(&ms)
}

/// One pill frame: where, how large, how bright.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Paint2 {
    /// Pill centre inside the buffer, in buffer pixels.
    pub cx: f32,
    pub cy: f32,
    /// Buffer pixels per CSS pixel of the pill: output scale times flash
    /// scale.
    pub k: f32,
    pub dot_alpha: f32,
    pub dot_scale: f32,
    /// Group opacity baked into the pixels.
    pub opacity: f32,
}

fn pill(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32) {
    // radius 999 px clamps to half the height
    let r = (h / 2.0).min(w / 2.0);
    const K: f32 = 0.552_284_8;
    let c = r * K;
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + c, y, x + w, y + r - c, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + c, x + w - r + c, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - c, y + h, x, y + h - r + c, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - c, x + r - c, y, x + r, y);
    pb.close();
}

/// Draw the pill into `buf` (`w` x `h` px, cleared first).
pub fn draw(buf: &mut [u8], w: usize, h: usize, layout: &Layout, p: &Paint2) {
    clear(buf);
    let k = p.k;
    let (pw, ph) = (layout.w as f32 * k, layout.h as f32 * k);
    let (x0, y0) = (p.cx - pw / 2.0, p.cy - ph / 2.0);
    {
        let Some(mut pm) = PixmapMut::from_bytes(buf, w as u32, h as u32) else {
            return;
        };
        let mut paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        let mut pb = PathBuilder::new();
        pill(&mut pb, x0, y0, pw, ph);
        if let Some(path) = pb.finish() {
            paint.set_color(bgra(255, 247, 242, 0.95));
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        // the 1 px border: the ring between the box and the box inset by it
        let mut pb = PathBuilder::new();
        pill(&mut pb, x0, y0, pw, ph);
        let b = BORDER * k;
        pill(&mut pb, x0 + b, y0 + b, pw - 2.0 * b, ph - 2.0 * b);
        if let Some(path) = pb.finish() {
            paint.set_color(bgra(CLAY.0, CLAY.1, CLAY.2, 0.2));
            pm.fill_path(
                &path,
                &paint,
                FillRule::EvenOdd,
                Transform::identity(),
                None,
            );
        }
        // the dot, centred on its 8 px box
        let dc = (x0 + (BORDER + PAD_X + DOT_PX / 2.0) * k, y0 + ph / 2.0);
        let r = DOT_PX / 2.0 * k * p.dot_scale;
        if let Some(path) = PathBuilder::from_circle(dc.0, dc.1, r) {
            paint.set_color(bgra(CLAY.0, CLAY.1, CLAY.2, p.dot_alpha));
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
    // the text: clay, glyph coverage composited source-over
    let f = font();
    let upm = f.units_per_em().unwrap_or(1000.0);
    let size = FONT_PX * k;
    let scale = PxScale::from(size * f.height_unscaled() / upm);
    let tx = x0 + (BORDER + PAD_X + DOT_PX + GAP) * k;
    let ty = y0 + (BORDER + PAD_Y) * k + layout.ascent * k;
    let (cr, cg, cb) = CLAY;
    for &(id, gx) in &layout.glyphs {
        let glyph = Glyph {
            id,
            scale,
            position: point(tx + gx * k, ty),
        };
        let Some(og) = f.outline_glyph(glyph) else {
            continue;
        };
        let b = og.px_bounds();
        let (bx, by) = (b.min.x as i32, b.min.y as i32);
        og.draw(|x, y, c| {
            let (px, py) = (bx + x as i32, by + y as i32);
            if px < 0 || py < 0 || px as usize >= w || py as usize >= h || c <= 0.0 {
                return;
            }
            let o = (py as usize * w + px as usize) * 4;
            let inv = 1.0 - c.min(1.0);
            let px = &mut buf[o..o + 4];
            px[0] = (f32::from(cb) * c + f32::from(px[0]) * inv).round() as u8;
            px[1] = (f32::from(cg) * c + f32::from(px[1]) * inv).round() as u8;
            px[2] = (f32::from(cr) * c + f32::from(px[2]) * inv).round() as u8;
            px[3] = (255.0 * c + f32::from(px[3]) * inv).round() as u8;
        });
    }
    apply_opacity(buf, p.opacity);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    fn layout() -> Layout {
        Layout::new(DEFAULT_TEXT)
    }

    #[test]
    fn layout_is_a_plausible_pill() {
        let l = layout();
        // 13 px medium sans, 29 characters: about 190 px of text
        assert!((200.0..300.0).contains(&l.w), "w = {}", l.w);
        assert!((30.0..40.0).contains(&l.h), "h = {}", l.h);
        let (bw, bh) = l.box_size();
        assert!(bw as f64 >= l.w * 1.4 && bh as f64 >= l.h * 1.4);
        // a longer text is wider, an empty-ish one narrower
        assert!(Layout::new("Claude is using your computer right now").w > l.w);
        assert!(Layout::new("Hi").w < l.w);
        // glyphs outside the subset (CJK) lay out as .notdef, never panic
        assert!(Layout::new("\u{4f7f}\u{7528}").w > 2.0 * 17.0);
    }

    #[test]
    fn clean_text_rules() {
        assert_eq!(clean_text("  hi\nthere\t"), Some("hi there".into()));
        assert_eq!(clean_text(" \n "), None);
        assert_eq!(clean_text(""), None);
        let long = "x".repeat(500);
        assert_eq!(clean_text(&long).unwrap().chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn flash_in_hold_and_slide() {
        let out = (1920.0, 1080.0);
        let (w, h) = (240.0, 36.0);
        // starts invisible at the centre
        let p0 = pose(0.0, out, w, h).unwrap();
        assert_eq!(p0.opacity, 0.0);
        assert!(close(p0.cx, 960.0, 1e-9) && close(p0.cy, 540.0, 1e-9));
        assert_eq!(p0.scale, SCALE_CENTER);
        // fully in at 5 % (300 ms), same place
        let p = pose(300.0, out, w, h).unwrap();
        assert!(close(p.opacity, 1.0, 1e-9) && close(p.cx, 960.0, 1e-9));
        // midway through the flash-in the ease is past linear? it is ease: 0.4,0,0.2,1
        let mid = pose(150.0, out, w, h).unwrap();
        assert!(mid.opacity > 0.0 && mid.opacity < 1.0);
        // holds until 88 %
        for ms in [300.0, 3000.0, 5279.0] {
            let p = pose(ms, out, w, h).unwrap();
            assert_eq!(p.opacity, 1.0, "{ms}");
            assert!(close(p.cx, 960.0, 1e-9) && close(p.cy, 540.0, 1e-9));
            assert_eq!(p.scale, SCALE_CENTER);
        }
        // sliding: opacity falls, the pill moves up and right, scale shrinks
        let a = pose(5400.0, out, w, h).unwrap();
        let b = pose(5800.0, out, w, h).unwrap();
        assert!(a.opacity < 1.0 && b.opacity < a.opacity);
        assert!(b.cx > a.cx && b.cy < a.cy && b.scale < a.scale);
        // just before the end: near the corner, almost invisible
        let e = pose(5999.9, out, w, h).unwrap();
        assert!(e.opacity < 0.01);
        assert!(close(e.scale, SCALE_CORNER, 0.01));
        // the corner keyframe: left = 100 % - 280 px, top = 40 px, unscaled
        // box, then scaled about its centre
        assert!(close(e.cx, 1920.0 - 280.0 + w / 2.0, 1.0));
        assert!(close(e.cy, 40.0 + h / 2.0, 0.5));
        // over
        assert_eq!(pose(6000.0, out, w, h), None);
        assert_eq!(pose(9000.0, out, w, h), None);
    }

    #[test]
    fn corner_keeps_a_long_pill_on_the_output() {
        let p = pose(5999.0, (1280.0, 720.0), 600.0, 36.0).unwrap();
        assert!(p.cx + p.scale * 600.0 / 2.0 <= 1280.0 - EDGE_MARGIN + 1e-9);
    }

    #[test]
    fn dot_pulses_between_full_and_half() {
        let (a0, s0) = dot(0.0);
        assert!(close(a0, 1.0, 1e-9) && close(s0, 1.0, 1e-9));
        let (a1, s1) = dot(900.0);
        assert!(close(a1, 0.5, 1e-9) && close(s1, 0.7, 1e-9));
        let (a2, _) = dot(1800.0);
        assert!(close(a2, 1.0, 1e-9));
        let (am, _) = dot(450.0);
        assert!(am < 1.0 && am > 0.5);
    }

    #[test]
    fn moving_phases() {
        assert!(moving(0.0) && moving(299.0));
        assert!(!moving(300.0) && !moving(5279.0));
        assert!(moving(5280.0) && moving(5999.0));
        assert!(!moving(6000.0));
    }

    fn alpha_at(buf: &[u8], w: usize, x: usize, y: usize) -> u8 {
        buf[(y * w + x) * 4 + 3]
    }

    #[test]
    fn draw_paints_a_pill_with_text_and_dot_inside_its_bounds() {
        let l = layout();
        let k = 1.5f32 * 1.4;
        let (bw, bh) = l.box_size();
        let (w, h) = ((bw as f32 * 1.5) as usize, (bh as f32 * 1.5) as usize);
        let mut buf = vec![0xaau8; w * h * 4];
        let p = Paint2 {
            cx: w as f32 / 2.0,
            cy: h as f32 / 2.0,
            k,
            dot_alpha: 1.0,
            dot_scale: 1.0,
            opacity: 1.0,
        };
        draw(&mut buf, w, h, &l, &p);
        // the corners of the buffer are untouched (cleared)
        assert_eq!(alpha_at(&buf, w, 0, 0), 0);
        assert_eq!(alpha_at(&buf, w, w - 1, h - 1), 0);
        // the pill's middle is the warm near-white base (0.95)
        let (cx, cy) = (w / 2, h / 2);
        let mut opaque = 0;
        let (mut minx, mut maxx, mut miny, mut maxy) = (w, 0, h, 0);
        for y in 0..h {
            for x in 0..w {
                if alpha_at(&buf, w, x, y) > 200 {
                    opaque += 1;
                    minx = minx.min(x);
                    maxx = maxx.max(x);
                    miny = miny.min(y);
                    maxy = maxy.max(y);
                }
            }
        }
        assert!(opaque > 1000);
        // the opaque extent is the pill: width = layout.w * k within 2 px
        let ext_w = (maxx - minx + 1) as f32;
        let ext_h = (maxy - miny + 1) as f32;
        assert!((ext_w - l.w as f32 * k).abs() < 3.0, "{ext_w}");
        assert!((ext_h - l.h as f32 * k).abs() < 3.0, "{ext_h}");
        // centred on the requested centre
        assert!(((minx + maxx) as f32 / 2.0 - cx as f32).abs() < 2.0);
        assert!(((miny + maxy) as f32 / 2.0 - cy as f32).abs() < 2.0);
        // clay text and dot exist: pixels whose red clearly exceeds blue
        // on a bright base are base-coloured (255,247,242 -> B<=R), clay is
        // (217,119,87): the green channel drops well below the base's
        let clay_px = (0..w * h)
            .filter(|&i| {
                let b = &buf[i * 4..i * 4 + 4];
                b[3] > 240 && b[1] < 140
            })
            .count();
        assert!(clay_px > 200, "text and dot pixels: {clay_px}");
    }

    #[test]
    fn opacity_scales_the_pixels_and_position_moves_the_pill() {
        let l = layout();
        let (w, h) = (500usize, 90usize);
        let mut a = vec![0u8; w * h * 4];
        let mut b = vec![0u8; w * h * 4];
        let mut p = Paint2 {
            cx: 250.0,
            cy: 45.0,
            k: 1.0,
            dot_alpha: 1.0,
            dot_scale: 1.0,
            opacity: 1.0,
        };
        draw(&mut a, w, h, &l, &p);
        p.opacity = 0.5;
        draw(&mut b, w, h, &l, &p);
        let sum = |v: &[u8]| v.chunks(4).map(|c| u64::from(c[3])).sum::<u64>();
        let (sa, sb) = (sum(&a), sum(&b));
        assert!(sb > sa * 45 / 100 && sb < sa * 55 / 100, "{sa} {sb}");
        p.opacity = 0.0;
        draw(&mut b, w, h, &l, &p);
        assert_eq!(sum(&b), 0);
        // a pill centred off to the side leaves the other edge clear
        p.opacity = 1.0;
        p.cx = 400.0;
        draw(&mut b, w, h, &l, &p);
        assert_eq!(alpha_at(&b, w, 0, 45), 0);
        assert!(alpha_at(&b, w, 400, 45) > 0);
    }

    #[test]
    fn drawing_never_leaves_the_buffer() {
        // a pill bigger than the buffer, and one partly outside: clipped,
        // no panic
        let l = layout();
        for (cx, cy) in [(0.0, 0.0), (100.0, 10.0), (-300.0, 400.0)] {
            let (w, h) = (120usize, 40usize);
            let mut buf = vec![0u8; w * h * 4];
            let p = Paint2 {
                cx,
                cy,
                k: 2.1,
                dot_alpha: 0.5,
                dot_scale: 0.7,
                opacity: 1.0,
            };
            draw(&mut buf, w, h, &l, &p);
        }
    }

    #[test]
    fn the_dot_scales_and_fades() {
        let l = layout();
        let (w, h) = (500usize, 90usize);
        let mut full = vec![0u8; w * h * 4];
        let mut small = vec![0u8; w * h * 4];
        let mk = |a, s| Paint2 {
            cx: 250.0,
            cy: 45.0,
            k: 1.0,
            dot_alpha: a,
            dot_scale: s,
            opacity: 1.0,
        };
        draw(&mut full, w, h, &l, &mk(1.0, 1.0));
        draw(&mut small, w, h, &l, &mk(0.5, 0.7));
        // the dot sits at x = left + (1 + 16 + 4); sample its centre
        let left = 250.0 - l.w as f32 / 2.0;
        let (dx, dy) = ((left + 21.0) as usize, 45usize);
        let g = |v: &[u8]| v[(dy * w + dx) * 4 + 1];
        assert!(g(&small) > g(&full), "a dimmer dot is closer to the base");
    }
}
