//! Outputs and coordinate conversion. Protocol coordinates are global
//! logical compositor coordinates; surfaces live in output-local logical
//! coordinates; buffers are in physical pixels at the surface's scale.

use serde::Serialize;

/// One output as reported in `ready`: name, logical position and size, and
/// the scale (physical / logical, fractional where the compositor uses one).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
}

impl OutputInfo {
    /// Global logical point -> output-local logical point.
    pub fn to_local(&self, gx: f64, gy: f64) -> (f64, f64) {
        (gx - self.x as f64, gy - self.y as f64)
    }

    /// Whether a global logical point lies on this output.
    pub fn contains(&self, gx: f64, gy: f64) -> bool {
        let (lx, ly) = self.to_local(gx, gy);
        lx >= 0.0 && ly >= 0.0 && lx < self.width as f64 && ly < self.height as f64
    }
}

/// A scale in 120ths (the wp_fractional_scale_v1 unit).
pub fn scale120(scale: f64) -> u32 {
    if !scale.is_finite() || scale <= 0.0 {
        return 120;
    }
    ((scale * 120.0).round() as u32).clamp(120 / 4, 120 * 8)
}

/// Physical pixels for a logical length at `scale120`, rounded half up (the
/// rounding the fractional-scale protocol prescribes for buffer sizes).
pub fn to_px(logical: i32, scale120: u32) -> i32 {
    ((logical as i64 * scale120 as i64 + 60) / 120) as i32
}

/// The scale of an output from its current mode (physical pixels) and its
/// logical size, `None` when either is unknown. `swap` is set for 90 and 270
/// degree transforms (the mode is in panel orientation).
pub fn output_scale(mode: (i32, i32), logical: (i32, i32), swap: bool) -> Option<f64> {
    let (mw, mh) = if swap { (mode.1, mode.0) } else { mode };
    if mw <= 0 || mh <= 0 || logical.0 <= 0 || logical.1 <= 0 {
        return None;
    }
    let s = mw as f64 / logical.0 as f64;
    // snap to 1/120 like the compositor's own fractional scale
    Some((s * 120.0).round() / 120.0)
}

/// Integer position of a subsurface whose hotspot (at `hot` inside it) should
/// sit on `local`, plus the fractional remainder the buffer draws itself.
pub fn place(local: f64, hot: i32) -> (i32, f64) {
    let base = local.floor();
    (base as i32 - hot, local - base)
}

/// A subsurface confined to its output: position and size in output-local
/// logical pixels, and where the anchor (hotspot or ring centre) lands
/// inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubBox {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub anchor_x: f64,
    pub anchor_y: f64,
}

/// Place a `size` square subsurface whose anchor (at `anchor`, `anchor`
/// inside it) sits on the output-local point `local`, clipped to the output
/// `0..out.0 x 0..out.1`. `None` when the point itself is not on the output:
/// then nothing is drawn (no part of a ring or shadow of a point that lives
/// on another output). A box straddling the output's edge is cut at the
/// edge, so a subsurface never reaches a neighbouring output.
pub fn sub_box(local: (f64, f64), anchor: i32, size: i32, out: (i32, i32)) -> Option<SubBox> {
    let (lx, ly) = local;
    let inside = |v: f64, n: i32| v.is_finite() && v >= 0.0 && v < n as f64;
    if !inside(lx, out.0) || !inside(ly, out.1) {
        return None;
    }
    let (px, fx) = place(lx, anchor);
    let (py, fy) = place(ly, anchor);
    let (x0, x1) = (px.max(0), (px + size).min(out.0));
    let (y0, y1) = (py.max(0), (py + size).min(out.1));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(SubBox {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
        anchor_x: anchor as f64 + fx - (x0 - px) as f64,
        anchor_y: anchor as f64 + fy - (y0 - py) as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, x: i32, y: i32, w: i32, h: i32, s: f64) -> OutputInfo {
        OutputInfo {
            name: name.into(),
            x,
            y,
            width: w,
            height: h,
            scale: s,
        }
    }

    #[test]
    fn global_to_local_on_a_second_output() {
        let a = out("A", 0, 0, 1280, 800, 1.0);
        let b = out("B", 1280, 0, 1280, 800, 1.5);
        assert_eq!(a.to_local(100.0, 50.0), (100.0, 50.0));
        assert_eq!(b.to_local(1380.0, 50.5), (100.0, 50.5));
        assert!(b.contains(1280.0, 0.0));
        assert!(!b.contains(1279.9, 0.0));
        assert!(!a.contains(1280.0, 0.0));
        // negative origins (an output left of / above the primary)
        let c = out("C", -1920, -200, 1920, 1080, 1.25);
        assert_eq!(c.to_local(-1900.0, -100.0), (20.0, 100.0));
        assert!(c.contains(-1.0, 879.0));
    }

    #[test]
    fn fractional_scale_buffers() {
        assert_eq!(scale120(1.5), 180);
        assert_eq!(scale120(1.25), 150);
        assert_eq!(scale120(0.0), 120);
        assert_eq!(scale120(f64::NAN), 120);
        assert_eq!(to_px(1280, 180), 1920);
        assert_eq!(to_px(853, 180), 1280); // 1279.5 rounds up
        assert_eq!(to_px(192, 150), 240);
        assert_eq!(to_px(1, 120), 1);
    }

    #[test]
    fn scale_from_mode() {
        assert_eq!(output_scale((1920, 1200), (1280, 800), false), Some(1.5));
        assert_eq!(output_scale((1200, 1920), (1280, 800), true), Some(1.5));
        assert_eq!(output_scale((2560, 1440), (2048, 1152), false), Some(1.25));
        assert_eq!(output_scale((0, 0), (1280, 800), false), None);
    }

    #[test]
    fn sub_box_inside_is_whole() {
        let b = sub_box((300.25, 200.5), 64, 192, (1280, 800)).unwrap();
        assert_eq!((b.x, b.y, b.w, b.h), (236, 136, 192, 192));
        assert_eq!((b.anchor_x, b.anchor_y), (64.25, 64.5));
    }

    #[test]
    fn sub_box_point_off_output_draws_nothing() {
        // shown on A (1600x1000 at 0,0); the point is on B (at 1600,0)
        let a = OutputInfo {
            name: "A".into(),
            x: 0,
            y: 0,
            width: 1600,
            height: 1000,
            scale: 1.0,
        };
        assert_eq!(
            sub_box(a.to_local(1700.0, 300.0), 64, 192, (1600, 1000)),
            None
        );
        assert_eq!(
            sub_box(a.to_local(1600.0, 300.0), 64, 192, (1600, 1000)),
            None
        );
        assert_eq!(sub_box((-0.01, 5.0), 64, 192, (1600, 1000)), None);
        assert_eq!(sub_box((5.0, 1000.0), 64, 192, (1600, 1000)), None);
        assert_eq!(sub_box((f64::NAN, 5.0), 64, 192, (1600, 1000)), None);
    }

    #[test]
    fn sub_box_straddling_is_cut_at_the_edge() {
        // right edge: the arrow box (hot 64 of 192) at x = 1590 would reach 1718
        let b = sub_box((1590.5, 300.0), 64, 192, (1600, 1000)).unwrap();
        assert_eq!((b.x, b.w), (1526, 74));
        assert_eq!(b.x + b.w, 1600);
        assert_eq!(b.anchor_x, 64.5);
        // left/top edge: the ring box (centre 56 of 112) at (10, 3)
        let b = sub_box((10.0, 3.0), 56, 112, (1600, 1000)).unwrap();
        assert_eq!((b.x, b.y, b.w, b.h), (0, 0, 66, 59));
        assert_eq!((b.anchor_x, b.anchor_y), (10.0, 3.0));
        // bottom-right corner
        let b = sub_box((1599.9, 999.9), 64, 192, (1600, 1000)).unwrap();
        assert_eq!((b.x + b.w, b.y + b.h), (1600, 1000));
        assert_eq!((b.w, b.h), (65, 65));
    }

    #[test]
    fn sub_box_on_a_second_scaled_output() {
        // B: 1280x800 logical at 1280,0, scale 1.5 (1920x1200 px)
        let b_out = OutputInfo {
            name: "B".into(),
            x: 1280,
            y: 0,
            width: 1280,
            height: 800,
            scale: 1.5,
        };
        let size = (b_out.width, b_out.height);
        let b = sub_box(b_out.to_local(1680.75, 300.0), 64, 192, size).unwrap();
        assert_eq!((b.x, b.y, b.w, b.h), (336, 236, 192, 192));
        assert_eq!(b.anchor_x, 64.75);
        // near B's left edge (the edge shared with A): cut, never negative
        let b = sub_box(b_out.to_local(1290.0, 300.0), 64, 192, size).unwrap();
        assert_eq!((b.x, b.w), (0, 138));
        assert_eq!(b.anchor_x, 10.0);
        // buffer size at 1.5 for the cut box
        assert_eq!(to_px(b.w, scale120(b_out.scale)), 207);
        // a point on A is not drawn on B
        assert_eq!(sub_box(b_out.to_local(1279.5, 300.0), 64, 192, size), None);
    }

    #[test]
    fn subsurface_placement() {
        assert_eq!(place(100.25, 64), (36, 0.25));
        assert_eq!(place(-0.5, 64), (-65, 0.5));
        let b = out("B", 1280, 0, 1280, 800, 1.5);
        let (lx, _) = b.to_local(1300.75, 0.0);
        assert_eq!(place(lx, 64), (-44, 0.75));
    }
}
