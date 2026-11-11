//! Upstream's motion, as pure functions of elapsed milliseconds: the press
//! ring, bloom and arrow bump, glide playback, the show/hide fade and the
//! glow pulse. Values are taken from upstream's cu-glow page (Claude Desktop
//! 5.1231): keyframes, durations and CSS easing curves.

use crate::protocol::Pose;

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function.
#[derive(Debug, Clone, Copy)]
pub struct CubicBezier {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

impl CubicBezier {
    pub const fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Progress at time fraction `x` (Newton, then bisection, like upstream).
    pub fn at(&self, x: f64) -> f64 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let cx = 3.0 * self.x1;
        let bx = 3.0 * (self.x2 - self.x1) - cx;
        let ax = 1.0 - cx - bx;
        let cy = 3.0 * self.y1;
        let by = 3.0 * (self.y2 - self.y1) - cy;
        let ay = 1.0 - cy - by;
        let fx = |t: f64| ((ax * t + bx) * t + cx) * t;
        let fy = |t: f64| ((ay * t + by) * t + cy) * t;
        let mut t = x;
        for _ in 0..8 {
            let err = fx(t) - x;
            if err.abs() < 1e-7 {
                return fy(t);
            }
            let d = (3.0 * ax * t + 2.0 * bx) * t + cx;
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        let (mut lo, mut hi) = (0.0, 1.0);
        t = x;
        while hi - lo > 1e-7 {
            if fx(t) < x {
                lo = t;
            } else {
                hi = t;
            }
            t = (lo + hi) / 2.0;
        }
        fy(t)
    }
}

/// CSS `ease-in-out`.
pub const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0.0, 0.58, 1.0);
/// Ring scale easing.
pub const RING_SCALE_EASE: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// Bloom scale easing.
pub const BLOOM_SCALE_EASE: CubicBezier = CubicBezier::new(0.33, 1.0, 0.68, 1.0);
/// Ring and bloom opacity easing.
pub const FADE_OUT_EASE: CubicBezier = CubicBezier::new(0.5, 1.0, 0.89, 1.0);

pub const RING_MS: f64 = 1100.0;
pub const BLOOM_MS: f64 = 560.0;
pub const BUMP_MS: f64 = 460.0;
/// Reduced motion: the ring only fades, at its rest size.
pub const RING_REDUCED_MS: f64 = 600.0;
/// `#overlay { transition: opacity 0.3s ease-in-out }`.
pub const FADE_MS: f64 = 300.0;
/// `glow-pulse 2s ease-in-out infinite`.
pub const PULSE_MS: f64 = 2000.0;

/// Ring and bloom geometry: a 68 px circle centred on the press point, the
/// ring a 2.5 px border inside it (box-sizing: border-box).
pub const RING_RADIUS: f64 = 34.0;
pub const RING_BORDER: f64 = 2.5;

/// One frame of the press animation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressFrame {
    pub ring_scale: f64,
    pub ring_alpha: f64,
    pub bloom_scale: f64,
    pub bloom_alpha: f64,
    /// Scale of the arrow around its hotspot (the "bump").
    pub arrow_scale: f64,
}

/// The arrow bump: 24 keyframes 20 ms apart (upstream generates them with a
/// cubic ease-in to 13 % smaller over 100 ms and a damped spring back).
pub fn bump_keyframes() -> [f64; 24] {
    let mut k = [1.0; 24];
    for (t, v) in k.iter_mut().enumerate() {
        let n = t as f64 * 20.0;
        let r = (n - 100.0) / 360.0;
        let i = (n / 100.0).powi(2) * (3.0 - n / 50.0);
        let a = if n < 100.0 {
            i
        } else {
            (-5.0 * r).exp() * (4.6 * r).cos()
        };
        if t != 23 {
            *v = ((1.0 - 0.13 * a) * 10000.0).round() / 10000.0;
        }
    }
    k
}

fn bump_at(ms: f64) -> f64 {
    let k = bump_keyframes();
    if ms <= 0.0 {
        return k[0];
    }
    let pos = ms / 20.0;
    let i = pos.floor() as usize;
    if i >= 23 {
        return 1.0;
    }
    let f = pos - i as f64;
    k[i] + (k[i + 1] - k[i]) * f
}

/// The press animation `ms` after the press, `None` once it is over.
pub fn press_frame(ms: f64, reduced_motion: bool) -> Option<PressFrame> {
    let ms = ms.max(0.0);
    if reduced_motion {
        if ms >= RING_REDUCED_MS {
            return None;
        }
        return Some(PressFrame {
            ring_scale: 1.0,
            ring_alpha: 0.7 * (1.0 - ms / RING_REDUCED_MS),
            bloom_scale: 1.0,
            bloom_alpha: 0.0,
            arrow_scale: 1.0,
        });
    }
    if ms >= RING_MS {
        return None;
    }
    let pr = ms / RING_MS;
    let (bloom_scale, bloom_alpha) = if ms < BLOOM_MS {
        let pb = ms / BLOOM_MS;
        (
            0.35 + 0.7 * BLOOM_SCALE_EASE.at(pb),
            0.5 * (1.0 - FADE_OUT_EASE.at(pb)),
        )
    } else {
        (1.05, 0.0)
    };
    Some(PressFrame {
        ring_scale: 0.25 + 1.2 * RING_SCALE_EASE.at(pr),
        ring_alpha: 0.7 * (1.0 - FADE_OUT_EASE.at(pr)),
        bloom_scale,
        bloom_alpha,
        arrow_scale: bump_at(ms),
    })
}

/// A glide being played: frames `frame_ms` apart, linearly interpolated
/// (WAAPI keyframes with linear timing, as upstream animates them). The last
/// frame is the resting pose.
#[derive(Debug, Clone, PartialEq)]
pub struct Glide {
    pub frames: Vec<Pose>,
    pub frame_ms: f64,
}

impl Glide {
    pub fn duration_ms(&self) -> f64 {
        (self.frames.len().saturating_sub(1)) as f64 * self.frame_ms
    }

    pub fn rest(&self) -> Pose {
        self.frames.last().copied().unwrap_or_default()
    }

    /// The pose `ms` into the glide.
    pub fn pose_at(&self, ms: f64) -> Pose {
        if self.frames.is_empty() {
            return Pose::default();
        }
        let pos = (ms.max(0.0)) / self.frame_ms;
        let i = pos.floor() as usize;
        if i + 1 >= self.frames.len() {
            return self.rest();
        }
        let (a, b) = (self.frames[i], self.frames[i + 1]);
        let f = pos - i as f64;
        let lerp = |p: f64, q: f64| p + (q - p) * f;
        Pose {
            x: lerp(a.x, b.x),
            y: lerp(a.y, b.y),
            deg: lerp(a.deg, b.deg),
            lift: lerp(a.lift, b.lift),
        }
    }
}

/// Opacity `ms` into a fade from `from` to `to` (300 ms ease-in-out).
pub fn fade(from: f64, to: f64, ms: f64) -> f64 {
    from + (to - from) * EASE_IN_OUT.at(ms / FADE_MS)
}

/// The glow is drawn at the pulse's peak (50 % keyframe) and dimmed with an
/// alpha multiplier; this is the multiplier at the 0 % / 100 % keyframe
/// (edge alpha of the base bands relative to the peak bands: 0.394 / 0.561).
pub const PULSE_LOW: f64 = 0.70;

/// Glow alpha multiplier `ms` into the pulse (period 2 s, ease-in-out each
/// half, low at 0 ms).
pub fn pulse(ms: f64) -> f64 {
    let p = (ms.max(0.0) % PULSE_MS) / PULSE_MS;
    let k = if p < 0.5 {
        EASE_IN_OUT.at(p * 2.0)
    } else {
        1.0 - EASE_IN_OUT.at((p - 0.5) * 2.0)
    };
    PULSE_LOW + (1.0 - PULSE_LOW) * k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn bezier_endpoints_and_linear() {
        let lin = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        for x in [0.1, 0.25, 0.5, 0.9] {
            assert!(close(lin.at(x), x, 1e-5));
        }
        assert_eq!(EASE_IN_OUT.at(0.0), 0.0);
        assert_eq!(EASE_IN_OUT.at(1.0), 1.0);
        assert!(close(EASE_IN_OUT.at(0.5), 0.5, 1e-5));
        // ease-out curves are ahead of linear early on
        assert!(RING_SCALE_EASE.at(0.2) > 0.6);
    }

    #[test]
    fn ring_keyframes() {
        let f0 = press_frame(0.0, false).unwrap();
        assert!(close(f0.ring_scale, 0.25, 1e-9));
        assert!(close(f0.ring_alpha, 0.7, 1e-9));
        assert!(close(f0.bloom_scale, 0.35, 1e-9));
        assert!(close(f0.bloom_alpha, 0.5, 1e-9));
        assert_eq!(f0.arrow_scale, 1.0);

        let end = press_frame(1099.0, false).unwrap();
        assert!(close(end.ring_scale, 1.45, 1e-3));
        assert!(end.ring_alpha < 0.01);
        assert_eq!(end.bloom_alpha, 0.0);
        assert!(press_frame(1100.0, false).is_none());

        // ring grows monotonically, fades monotonically
        let mut last = press_frame(0.0, false).unwrap();
        for ms in (10..1100).step_by(10) {
            let f = press_frame(ms as f64, false).unwrap();
            assert!(f.ring_scale >= last.ring_scale);
            assert!(f.ring_alpha <= last.ring_alpha);
            last = f;
        }
        // bloom over after 560 ms
        assert_eq!(press_frame(560.0, false).unwrap().bloom_alpha, 0.0);
    }

    #[test]
    fn bump_matches_upstream() {
        let k = bump_keyframes();
        assert_eq!(k[0], 1.0);
        assert_eq!(k[5], 0.87); // n=100: full 13 % squeeze
        assert_eq!(k[23], 1.0);
        assert_ne!(k[10], 1.0);
        assert!(close(
            press_frame(100.0, false).unwrap().arrow_scale,
            0.87,
            1e-9
        ));
        assert!(close(
            press_frame(110.0, false).unwrap().arrow_scale,
            (k[5] + k[6]) / 2.0,
            1e-9
        ));
        assert_eq!(press_frame(500.0, false).unwrap().arrow_scale, 1.0);
    }

    #[test]
    fn reduced_motion_ring() {
        let f = press_frame(300.0, true).unwrap();
        assert_eq!(f.ring_scale, 1.0);
        assert!(close(f.ring_alpha, 0.35, 1e-9));
        assert_eq!(f.bloom_alpha, 0.0);
        assert_eq!(f.arrow_scale, 1.0);
        assert!(press_frame(600.0, true).is_none());
    }

    #[test]
    fn glide_playback_timing() {
        let g = Glide {
            frames: vec![
                Pose::at(0.0, 0.0),
                Pose {
                    x: 10.0,
                    y: 20.0,
                    deg: 4.0,
                    lift: 1.0,
                },
                Pose::at(30.0, 20.0),
            ],
            frame_ms: 16.0,
        };
        assert_eq!(g.duration_ms(), 32.0);
        assert_eq!(g.pose_at(0.0), Pose::at(0.0, 0.0));
        let p = g.pose_at(8.0);
        assert!(close(p.x, 5.0, 1e-9) && close(p.y, 10.0, 1e-9));
        assert!(close(p.deg, 2.0, 1e-9) && close(p.lift, 0.5, 1e-9));
        assert_eq!(g.pose_at(16.0).x, 10.0);
        assert!(close(g.pose_at(24.0).x, 20.0, 1e-9));
        assert!(close(g.pose_at(24.0).lift, 0.5, 1e-9));
        assert_eq!(g.pose_at(32.0), Pose::at(30.0, 20.0));
        assert_eq!(g.pose_at(1e6), Pose::at(30.0, 20.0));
        assert_eq!(g.pose_at(-5.0), Pose::at(0.0, 0.0));
        let one = Glide {
            frames: vec![Pose::at(3.0, 4.0)],
            frame_ms: 16.0,
        };
        assert_eq!(one.duration_ms(), 0.0);
        assert_eq!(one.pose_at(0.0), Pose::at(3.0, 4.0));
    }

    #[test]
    fn fade_and_pulse() {
        assert_eq!(fade(0.0, 1.0, 0.0), 0.0);
        assert!(close(fade(0.0, 1.0, 150.0), 0.5, 1e-5));
        assert_eq!(fade(0.0, 1.0, 300.0), 1.0);
        assert_eq!(fade(1.0, 0.0, 400.0), 0.0);
        assert!(close(pulse(0.0), PULSE_LOW, 1e-9));
        assert!(close(pulse(1000.0), 1.0, 1e-9));
        assert!(close(pulse(2000.0), PULSE_LOW, 1e-9));
        assert!(close(pulse(500.0), (PULSE_LOW + 1.0) / 2.0, 1e-5));
        for ms in (0..4000).step_by(37) {
            let m = pulse(ms as f64);
            assert!((PULSE_LOW - 1e-9..=1.0 + 1e-9).contains(&m));
        }
    }
}
