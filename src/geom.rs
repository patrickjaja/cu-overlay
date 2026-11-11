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
    fn subsurface_placement() {
        assert_eq!(place(100.25, 64), (36, 0.25));
        assert_eq!(place(-0.5, 64), (-65, 0.5));
        let b = out("B", 1280, 0, 1280, 800, 1.5);
        let (lx, _) = b.to_local(1300.75, 0.0);
        assert_eq!(place(lx, 64), (-44, 0.75));
    }
}
