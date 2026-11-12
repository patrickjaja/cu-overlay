//! The X11 backend's output model, as pure functions of what the X server
//! reports (RandR monitors, the root size, root properties and XSETTINGS),
//! so every mapping rule is unit tested without a server.
//!
//! The protocol speaks global LOGICAL compositor coordinates. Under XWayland
//! on Mutter there are two cases:
//!
//! - without the experimental `xwayland-native-scaling`, X coordinates are
//!   the logical coordinates (factor 1; Mutter upscales X windows, so they
//!   are blurry on scaled monitors);
//! - with it, X coordinates are logical x an integer factor `s` (the ceiling
//!   of the largest monitor scale) on EVERY monitor, and X clients are told
//!   so through XSETTINGS `Gdk/WindowScalingFactor` and `Xft.dpi` = 96 s.
//!
//! The factor comes from XSETTINGS `Gdk/WindowScalingFactor` (exact), else
//! from `Xft.dpi` / 96 when that is a whole number (a text scaling factor
//! makes it fractional; then 1), and a caller may force it (Mutter's
//! physical layout mode looks identical from X, see [`compute`]).
//!
//! After a live change of the factor (native scaling switched, the largest
//! monitor scale changed) Mutter updates XSETTINGS, `Xft.dpi`, the work
//! areas and `_NET_DESKTOP_GEOMETRY`, but XWayland's RandR keeps the old
//! factor for every value Mutter did not resend: measured on Mutter 48.1, a
//! monitor whose logical geometry did not change keeps its old position and
//! size, and one whose scale changed gets a new size at the new factor but
//! keeps its old position. Each RandR monitor is therefore fitted to its
//! work area (`_GTK_WORKAREAS_D0`, one per monitor, always current): size
//! and position are each RandR's times `s / b` (or unchanged), the smallest
//! size that contains the work area, a position that keeps it inside, no
//! two monitors overlapping ([`fit_workareas`]). Without work areas (not
//! Mutter) the uniform ratio `_NET_DESKTOP_GEOMETRY` / root size is used
//! ([`stale_ratio`]).

use crate::geom::OutputInfo;

/// Largest factor accepted from the server (XSETTINGS, Xft.dpi, a ratio).
pub const MAX_SCALE: i32 = 8;

/// A rectangle in X (root window) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One RandR monitor as the server reports it (X pixels).
#[derive(Debug, Clone, PartialEq)]
pub struct RawMonitor {
    /// RandR monitor name (the connector, e.g. `Virtual-1`, `DP-1`).
    pub name: String,
    pub rect: XRect,
}

/// Everything the mapping depends on, read from the server.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawLayout {
    pub monitors: Vec<RawMonitor>,
    /// The root window's current size.
    pub root: (i32, i32),
    /// `_NET_DESKTOP_GEOMETRY` (Mutter keeps it in the current X
    /// coordinates even while RandR is stale).
    pub desktop: Option<(i32, i32)>,
    /// XSETTINGS `Gdk/WindowScalingFactor`.
    pub window_scaling: Option<i32>,
    /// `Xft.dpi` from `RESOURCE_MANAGER`.
    pub xft_dpi: Option<f64>,
    /// `_GTK_WORKAREAS_D0`: Mutter's work area of each monitor, current X
    /// coordinates, in Mutter's monitor order.
    pub workareas: Vec<XRect>,
}

/// How RandR geometry was corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correction {
    /// Fitted to the work areas ([`fit_workareas`]).
    WorkAreas,
    /// Scaled uniformly by a/b ([`stale_ratio`]).
    Ratio(i32, i32),
}

/// An output: what the protocol reports (logical) and where its windows go.
#[derive(Debug, Clone, PartialEq)]
pub struct XOutput {
    pub info: OutputInfo,
    /// The output in X pixels: the logical rectangle times the factor.
    pub rect: XRect,
}

/// The mapped layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// X pixels per logical pixel (integer, >= 1); also the buffer scale.
    pub scale: i32,
    /// `Some` when RandR was stale and its geometry corrected.
    pub corrected: Option<Correction>,
    pub outputs: Vec<XOutput>,
}

impl Layout {
    pub fn output(&self, name: &str) -> Option<&XOutput> {
        self.outputs.iter().find(|o| o.info.name == name)
    }

    pub fn infos(&self) -> Vec<OutputInfo> {
        self.outputs.iter().map(|o| o.info.clone()).collect()
    }
}

/// The factor from X pixels to logical pixels: `forced`, else XSETTINGS
/// `Gdk/WindowScalingFactor`, else a whole-number `Xft.dpi / 96`, else 1.
pub fn scale_factor(raw: &RawLayout, forced: Option<i32>) -> i32 {
    if let Some(f) = forced.filter(|f| (1..=MAX_SCALE).contains(f)) {
        return f;
    }
    if let Some(f) = raw.window_scaling.filter(|f| (1..=MAX_SCALE).contains(f)) {
        return f;
    }
    if let Some(dpi) = raw.xft_dpi.filter(|d| d.is_finite() && *d > 0.0) {
        let k = dpi / 96.0;
        let r = k.round();
        if (k - r).abs() < 0.01 && (1.0..=MAX_SCALE as f64).contains(&r) {
            return r as i32;
        }
    }
    1
}

fn gcd(a: i32, b: i32) -> i32 {
    if b == 0 { a.abs() } else { gcd(b, a % b) }
}

/// How stale RandR is: `Some((a, b))` when `_NET_DESKTOP_GEOMETRY` is a/b
/// times the root size on both axes (a, b <= [`MAX_SCALE`], a/b != 1,
/// within 2 %). `None` when they agree or the ratio makes no sense.
pub fn stale_ratio(root: (i32, i32), desktop: Option<(i32, i32)>) -> Option<(i32, i32)> {
    let (dw, dh) = desktop?;
    let (rw, rh) = root;
    if rw <= 0 || rh <= 0 || dw <= 0 || dh <= 0 || (dw, dh) == (rw, rh) {
        return None;
    }
    let kx = dw as f64 / rw as f64;
    let ky = dh as f64 / rh as f64;
    if (kx - ky).abs() > 0.02 * kx.max(ky) {
        return None;
    }
    let k = (kx + ky) / 2.0;
    let mut best: Option<(f64, (i32, i32))> = None;
    for a in 1..=MAX_SCALE {
        for b in 1..=MAX_SCALE {
            if a == b || gcd(a, b) != 1 {
                continue;
            }
            let err = (a as f64 / b as f64 - k).abs() / k;
            if best.is_none_or(|(e, _)| err < e) {
                best = Some((err, (a, b)));
            }
        }
    }
    best.filter(|(e, _)| *e < 0.02).map(|(_, r)| r)
}

/// `v * a / b` when that is a whole number.
fn scaled(v: i32, (a, b): (i32, i32)) -> Option<i32> {
    let n = v as i64 * a as i64;
    (n % b as i64 == 0).then(|| (n / b as i64) as i32)
}

fn overlap(a: &XRect, b: &XRect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// The candidate factors for a stale value: `s / b` for b in 1..=8, and
/// 1, ascending.
fn factors(s: i32) -> Vec<(i32, i32)> {
    let mut ks: Vec<(i32, i32)> = (1..=MAX_SCALE)
        .map(|b| (s / gcd(s, b), b / gcd(s, b)))
        .chain([(1, 1)])
        .collect();
    ks.sort_by(|p, q| (p.0 * q.1).cmp(&(q.0 * p.1)));
    ks.dedup();
    ks
}

/// One monitor fitted to one work area: the smallest candidate size that
/// contains it, then a position (at the size's factor, else unchanged,
/// else any) that keeps it inside; values are multiples of `s`.
fn fit_one(r: XRect, wa: XRect, s: i32, ks: &[(i32, i32)]) -> Option<XRect> {
    if wa.w <= 0 || wa.h <= 0 || r.w <= 0 || r.h <= 0 {
        return None;
    }
    let (k, w, h) = ks.iter().find_map(|&k| {
        let (w, h) = (scaled(r.w, k)?, scaled(r.h, k)?);
        (w >= wa.w && h >= wa.h && w % s == 0 && h % s == 0).then_some((k, w, h))
    })?;
    let at = |k: (i32, i32)| {
        let (x, y) = (scaled(r.x, k)?, scaled(r.y, k)?);
        (x <= wa.x
            && y <= wa.y
            && x + w >= wa.x + wa.w
            && y + h >= wa.y + wa.h
            && x % s == 0
            && y % s == 0)
            .then_some(XRect { x, y, w, h })
    };
    at(k)
        .or_else(|| at((1, 1)))
        .or_else(|| ks.iter().find_map(|&k| at(k)))
}

/// Fit each RandR monitor to its work area (see the module docs). The work
/// areas come in Mutter's monitor order, so every pairing is tried (up to
/// six monitors; more are paired in (x, y) order) and the valid one that
/// changes the fewest values wins; valid means every monitor contains its
/// work area and no two overlap. `None` when the counts differ or nothing
/// fits (the caller then falls back to [`stale_ratio`]).
pub fn fit_workareas(mons: &[RawMonitor], was: &[XRect], s: i32) -> Option<Vec<XRect>> {
    let n = mons.len();
    if n == 0 || n != was.len() {
        return None;
    }
    let ks = factors(s);
    let eval = |perm: &[usize]| -> Option<(usize, Vec<XRect>)> {
        let out = perm
            .iter()
            .enumerate()
            .map(|(m, &w)| fit_one(mons[m].rect, was[w], s, &ks))
            .collect::<Option<Vec<_>>>()?;
        for (i, a) in out.iter().enumerate() {
            if out[i + 1..].iter().any(|b| overlap(a, b)) {
                return None;
            }
        }
        let changed = out
            .iter()
            .zip(mons)
            .map(|(a, m)| {
                let r = m.rect;
                [a.x != r.x, a.y != r.y, a.w != r.w, a.h != r.h]
                    .iter()
                    .filter(|c| **c)
                    .count()
            })
            .sum();
        Some((changed, out))
    };
    if n > 6 {
        let mut mi: Vec<usize> = (0..n).collect();
        mi.sort_by_key(|&i| (mons[i].rect.x, mons[i].rect.y));
        let mut wi: Vec<usize> = (0..n).collect();
        wi.sort_by_key(|&i| (was[i].x, was[i].y));
        let mut perm = vec![0; n];
        for (&m, &w) in mi.iter().zip(&wi) {
            perm[m] = w;
        }
        return eval(&perm).map(|(_, f)| f);
    }
    let mut best: Option<(usize, Vec<XRect>)> = None;
    let mut perm: Vec<usize> = (0..n).collect();
    permute(&mut perm, 0, &mut |p| {
        if let Some(c) = eval(p)
            && best.as_ref().is_none_or(|b| c.0 < b.0)
        {
            best = Some(c);
        }
    });
    best.map(|(_, f)| f)
}

fn permute(p: &mut [usize], k: usize, f: &mut dyn FnMut(&[usize])) {
    if k == p.len() {
        return f(p);
    }
    for i in k..p.len() {
        p.swap(k, i);
        permute(p, k + 1, f);
        p.swap(k, i);
    }
}

/// Map the server's view to the protocol's: each RandR monitor (corrected
/// when RandR is stale, see the module docs) divided by the factor gives
/// the logical rectangle reported in `ready` / `outputs` with `scale` = the
/// factor; its windows go at that rectangle times the factor, so a global
/// logical point `L` lands on X pixel `round(L * s)`.
///
/// Mutter's physical layout mode (`scale-monitor-framebuffer` off) with an
/// integer scale > 1 looks the same from X as native scaling (XSETTINGS
/// says 2, X pixels are physical) but its layout IS physical pixels; a
/// caller that knows (the GNOME bridge reads `layout-mode` from
/// DisplayConfig) forces the factor to 1 there.
pub fn compute(raw: &RawLayout, forced: Option<i32>) -> Layout {
    let s = scale_factor(raw, forced);
    let randr: Vec<XRect> = raw.monitors.iter().map(|m| m.rect).collect();
    let (rects, corrected) = match fit_workareas(&raw.monitors, &raw.workareas, s) {
        Some(f) if f != randr => (f, Some(Correction::WorkAreas)),
        Some(f) => (f, None),
        None => match stale_ratio(raw.root, raw.desktop) {
            Some((a, b)) => {
                let fix = |v: i32| ((v as i64 * a as i64) as f64 / b as f64).round() as i32;
                let f = randr
                    .iter()
                    .map(|r| XRect {
                        x: fix(r.x),
                        y: fix(r.y),
                        w: fix(r.w),
                        h: fix(r.h),
                    })
                    .collect();
                (f, Some(Correction::Ratio(a, b)))
            }
            None => (randr, None),
        },
    };
    let to_l = |v: i32| (v as f64 / s as f64).round() as i32;
    let outputs = raw
        .monitors
        .iter()
        .zip(rects)
        .filter_map(|(m, r)| {
            let (lx, ly, lw, lh) = (to_l(r.x), to_l(r.y), to_l(r.w), to_l(r.h));
            if lw <= 0 || lh <= 0 || m.name.is_empty() {
                return None;
            }
            Some(XOutput {
                info: OutputInfo {
                    name: m.name.clone(),
                    x: lx,
                    y: ly,
                    width: lw,
                    height: lh,
                    scale: s as f64,
                },
                rect: XRect {
                    x: lx * s,
                    y: ly * s,
                    w: lw * s,
                    h: lh * s,
                },
            })
        })
        .collect();
    Layout {
        scale: s,
        corrected,
        outputs,
    }
}

/// `Xft.dpi` from a `RESOURCE_MANAGER` string.
pub fn parse_xft_dpi(resources: &[u8]) -> Option<f64> {
    String::from_utf8_lossy(resources).lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "Xft.dpi").then(|| v.trim().parse::<f64>().ok())?
    })
}

/// An integer setting from an `_XSETTINGS_SETTINGS` property (the XSETTINGS
/// wire format: byte order, serial, count, then typed entries).
pub fn parse_xsettings_int(data: &[u8], key: &str) -> Option<i32> {
    let le = match *data.first()? {
        0 => true,
        1 => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b: [u8; 2] = data.get(o..o + 2)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(o..o + 4)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let pad = |n: usize| (n + 3) & !3;
    let n = u32_at(8)? as usize;
    let mut o = 12;
    for _ in 0..n.min(4096) {
        let kind = *data.get(o)?;
        let name_len = u16_at(o + 2)? as usize;
        let name = data.get(o + 4..o + 4 + name_len)?;
        o += 4 + pad(name_len) + 4; // name, last-change serial
        match kind {
            0 => {
                if name == key.as_bytes() {
                    return Some(u32_at(o)? as i32);
                }
                o += 4;
            }
            1 => {
                let len = u32_at(o)? as usize;
                o += 4 + pad(len);
            }
            2 => o += 8,
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, x: i32, y: i32, w: i32, h: i32) -> RawMonitor {
        RawMonitor {
            name: name.into(),
            rect: XRect { x, y, w, h },
        }
    }

    /// The VM layout: Virtual-1 1280x800 at scale 1, Virtual-2 1600x1000
    /// at 1.25 to its right (logical 1280x800 at 1280,0).
    fn two(s: i32) -> Vec<RawMonitor> {
        vec![
            mon("Virtual-1", 0, 0, 1280 * s, 800 * s),
            mon("Virtual-2", 1280 * s, 0, 1280 * s, 800 * s),
        ]
    }

    fn infos(l: &Layout) -> Vec<(String, i32, i32, i32, i32, f64)> {
        l.outputs
            .iter()
            .map(|o| {
                let i = &o.info;
                (i.name.clone(), i.x, i.y, i.width, i.height, i.scale)
            })
            .collect()
    }

    #[test]
    fn without_native_scaling_x_is_logical() {
        // measured on mutter 48.1: XSETTINGS 1, Xft.dpi 96, RandR = logical
        let raw = RawLayout {
            monitors: two(1),
            root: (2560, 800),
            desktop: Some((2560, 800)),
            window_scaling: Some(1),
            xft_dpi: Some(96.0),
            workareas: vec![wa(66, 29, 1214, 771), wa(1280, 0, 1280, 800)],
        };
        let l = compute(&raw, None);
        assert_eq!(l.scale, 1);
        assert_eq!(l.corrected, None);
        assert_eq!(
            infos(&l),
            vec![
                ("Virtual-1".into(), 0, 0, 1280, 800, 1.0),
                ("Virtual-2".into(), 1280, 0, 1280, 800, 1.0)
            ]
        );
        assert_eq!(
            l.outputs[1].rect,
            XRect {
                x: 1280,
                y: 0,
                w: 1280,
                h: 800
            }
        );
    }

    #[test]
    fn native_scaling_divides_every_monitor_by_the_factor() {
        // after an XWayland restart with native scaling: X = logical x 2
        let raw = RawLayout {
            monitors: two(2),
            root: (5120, 1600),
            desktop: Some((5120, 1600)),
            window_scaling: Some(2),
            xft_dpi: Some(192.0),
            workareas: vec![wa(132, 58, 2428, 1542), wa(2560, 0, 2560, 1600)],
        };
        let l = compute(&raw, None);
        assert_eq!(l.scale, 2);
        assert_eq!(l.corrected, None);
        assert_eq!(
            infos(&l),
            vec![
                ("Virtual-1".into(), 0, 0, 1280, 800, 2.0),
                ("Virtual-2".into(), 1280, 0, 1280, 800, 2.0)
            ]
        );
        assert_eq!(
            l.outputs[1].rect,
            XRect {
                x: 2560,
                y: 0,
                w: 2560,
                h: 1600
            }
        );
    }

    fn wa(x: i32, y: i32, w: i32, h: i32) -> XRect {
        XRect { x, y, w, h }
    }

    #[test]
    fn stale_randr_after_a_live_switch_is_corrected() {
        // measured: native scaling switched on live; XSETTINGS, Xft.dpi and
        // _NET_DESKTOP_GEOMETRY moved to factor 2, RandR and the root stayed
        let mut raw = RawLayout {
            monitors: two(1),
            root: (2560, 800),
            desktop: Some((5120, 1600)),
            window_scaling: Some(2),
            xft_dpi: Some(192.0),
            workareas: vec![wa(132, 58, 2428, 1542), wa(2560, 0, 2560, 1600)],
        };
        let l = compute(&raw, None);
        assert_eq!((l.scale, l.corrected), (2, Some(Correction::WorkAreas)));
        assert_eq!(
            infos(&l),
            vec![
                ("Virtual-1".into(), 0, 0, 1280, 800, 2.0),
                ("Virtual-2".into(), 1280, 0, 1280, 800, 2.0)
            ]
        );
        // the same without work areas: the uniform desktop / root ratio
        raw.workareas.clear();
        let l = compute(&raw, None);
        assert_eq!((l.scale, l.corrected), (2, Some(Correction::Ratio(2, 1))));
        assert_eq!(
            infos(&l),
            vec![
                ("Virtual-1".into(), 0, 0, 1280, 800, 2.0),
                ("Virtual-2".into(), 1280, 0, 1280, 800, 2.0)
            ]
        );
        assert_eq!(
            l.outputs[1].rect,
            XRect {
                x: 2560,
                y: 0,
                w: 2560,
                h: 1600
            }
        );
        // and back: native scaling switched off live, RandR still at 2
        let mut raw = RawLayout {
            monitors: two(2),
            root: (5120, 1600),
            desktop: Some((2560, 800)),
            window_scaling: Some(1),
            xft_dpi: Some(96.0),
            workareas: vec![wa(66, 29, 1214, 771), wa(1280, 0, 1280, 800)],
        };
        let l = compute(&raw, None);
        assert_eq!((l.scale, l.corrected), (1, Some(Correction::WorkAreas)));
        assert_eq!(
            l.outputs[1].rect,
            XRect {
                x: 1280,
                y: 0,
                w: 1280,
                h: 800
            }
        );
        raw.workareas.clear();
        let l = compute(&raw, None);
        assert_eq!((l.scale, l.corrected), (1, Some(Correction::Ratio(1, 2))));
        assert_eq!(
            l.outputs[1].rect,
            XRect {
                x: 1280,
                y: 0,
                w: 1280,
                h: 800
            }
        );
        assert_eq!(l.outputs[1].info.width, 1280);
        // a 2 -> 3 change (a monitor at 2.25 now): ratio 3/2
        assert_eq!(stale_ratio((5120, 1600), Some((7680, 2400))), Some((3, 2)));
    }

    #[test]
    fn randr_stale_per_monitor_and_per_field_is_fitted_to_the_work_areas() {
        // measured after a fresh login with native scaling (Virtual-2 at 1.0)
        // and then Virtual-2 set to 1.25: Virtual-1 (unchanged) kept its
        // factor-1 geometry, Virtual-2 got its new size at factor 2 but kept
        // its factor-1 position; root 3840x1600, the desktop 5120x1600
        let raw = RawLayout {
            monitors: vec![
                mon("Virtual-1", 0, 0, 1280, 800),
                mon("Virtual-2", 1280, 0, 2560, 1600),
            ],
            root: (3840, 1600),
            desktop: Some((5120, 1600)),
            window_scaling: Some(2),
            xft_dpi: Some(192.0),
            workareas: vec![wa(132, 58, 2428, 1542), wa(2560, 0, 2560, 1600)],
        };
        assert_eq!(stale_ratio(raw.root, raw.desktop), None, "not uniform");
        let l = compute(&raw, None);
        assert_eq!(l.corrected, Some(Correction::WorkAreas));
        assert_eq!(
            infos(&l),
            vec![
                ("Virtual-1".into(), 0, 0, 1280, 800, 2.0),
                ("Virtual-2".into(), 1280, 0, 1280, 800, 2.0)
            ]
        );
        assert_eq!(
            l.outputs[1].rect,
            XRect {
                x: 2560,
                y: 0,
                w: 2560,
                h: 1600
            }
        );
        // the work areas come in Mutter's order, not RandR's
        let mut r2 = raw.clone();
        r2.workareas.reverse();
        r2.monitors.reverse();
        assert_eq!(
            compute(&r2, None).output("Virtual-2"),
            l.output("Virtual-2")
        );
    }

    #[test]
    fn work_area_fit_with_panels_and_vertical_layouts() {
        // up to date RandR (factor 1), a 1000 px tall monitor next to an 800
        // px one, top bar and dock on the first: nothing to correct
        let mons = vec![mon("A", 0, 0, 1280, 800), mon("B", 1280, 0, 1600, 1000)];
        let was = vec![wa(66, 29, 1214, 771), wa(1280, 0, 1600, 1000)];
        assert_eq!(
            fit_workareas(&mons, &was, 1),
            Some(vec![wa(0, 0, 1280, 800), wa(1280, 0, 1600, 1000)])
        );
        // stacked vertically, the lower one stale at factor 1 (truth 2)
        let mons = vec![mon("A", 0, 0, 2560, 1600), mon("B", 0, 800, 1280, 800)];
        let was = vec![wa(132, 58, 2428, 1542), wa(0, 1600, 2560, 1600)];
        assert_eq!(
            fit_workareas(&mons, &was, 2),
            Some(vec![wa(0, 0, 2560, 1600), wa(0, 1600, 2560, 1600)])
        );
        // a monitor left of the primary (negative x), stale at factor 1
        let mons = vec![mon("A", 0, 0, 1280, 800), mon("L", -1280, 0, 1280, 800)];
        let was = vec![wa(132, 58, 2428, 1542), wa(-2560, 0, 2560, 1600)];
        assert_eq!(
            fit_workareas(&mons, &was, 2),
            Some(vec![wa(0, 0, 2560, 1600), wa(-2560, 0, 2560, 1600)])
        );
        // count mismatch or no consistent fit: none (the caller falls back)
        assert_eq!(fit_workareas(&mons, &was[..1], 2), None);
        assert_eq!(
            fit_workareas(&mons, &[wa(0, 0, 9000, 9000), wa(0, 0, 1, 1)], 2),
            None
        );
        assert_eq!(fit_workareas(&[], &[], 1), None);
    }

    #[test]
    fn stale_ratio_rejects_nonsense() {
        assert_eq!(stale_ratio((2560, 800), None), None);
        assert_eq!(stale_ratio((2560, 800), Some((2560, 800))), None);
        // the axes disagree: not a uniform rescale
        assert_eq!(stale_ratio((2560, 800), Some((5120, 800))), None);
        // not a ratio of small integers
        assert_eq!(stale_ratio((1000, 1000), Some((1013, 1013))), None);
        assert_eq!(stale_ratio((0, 800), Some((10, 10))), None);
    }

    #[test]
    fn factor_sources_in_order() {
        let mut raw = RawLayout {
            window_scaling: Some(2),
            xft_dpi: Some(96.0),
            ..RawLayout::default()
        };
        assert_eq!(scale_factor(&raw, None), 2, "XSETTINGS wins");
        assert_eq!(scale_factor(&raw, Some(1)), 1, "forced wins");
        assert_eq!(scale_factor(&raw, Some(0)), 2, "invalid force ignored");
        raw.window_scaling = None;
        raw.xft_dpi = Some(192.0);
        assert_eq!(scale_factor(&raw, None), 2, "Xft.dpi 192");
        // text scaling 1.25 without XSETTINGS: fractional, so factor 1
        raw.xft_dpi = Some(120.0);
        assert_eq!(scale_factor(&raw, None), 1);
        raw.xft_dpi = None;
        assert_eq!(scale_factor(&raw, None), 1);
        raw.window_scaling = Some(99);
        assert_eq!(scale_factor(&raw, None), 1, "out of range ignored");
    }

    #[test]
    fn multi_monitor_negative_and_vertical() {
        // a monitor left of and one below the primary, native factor 2
        let raw = RawLayout {
            monitors: vec![
                mon("DP-1", 0, 0, 3840, 2160),
                mon("HDMI-1", -2560, 0, 2560, 1440),
                mon("eDP-1", 0, 2160, 2880, 1800),
            ],
            root: (6400, 3960),
            desktop: Some((6400, 3960)),
            window_scaling: Some(2),
            xft_dpi: None,
            workareas: Vec::new(),
        };
        let l = compute(&raw, None);
        assert_eq!(
            infos(&l),
            vec![
                ("DP-1".into(), 0, 0, 1920, 1080, 2.0),
                ("HDMI-1".into(), -1280, 0, 1280, 720, 2.0),
                ("eDP-1".into(), 0, 1080, 1440, 900, 2.0)
            ]
        );
        assert_eq!(
            l.outputs[1].rect,
            XRect {
                x: -2560,
                y: 0,
                w: 2560,
                h: 1440
            }
        );
        // a point maps as X = round(L * s)
        let o = &l.outputs[2];
        let (gx, gy) = (700.25, 1500.5);
        let (lx, ly) = o.info.to_local(gx, gy);
        assert_eq!(
            (o.rect.x as f64 + lx * 2.0, o.rect.y as f64 + ly * 2.0),
            (1400.5, 3001.0)
        );
        // a zero-sized monitor is dropped
        let raw = RawLayout {
            monitors: vec![mon("X", 0, 0, 0, 0), mon("Y", 0, 0, 10, 10)],
            root: (10, 10),
            ..RawLayout::default()
        };
        assert_eq!(compute(&raw, None).outputs.len(), 1);
    }

    #[test]
    fn xft_dpi_from_resources() {
        let rm = b"*customization:\t-color\nXcursor.size:\t48\nXft.antialias:\t1\nXft.dpi:\t192\nXft.hinting:\t1\n";
        assert_eq!(parse_xft_dpi(rm), Some(192.0));
        assert_eq!(parse_xft_dpi(b"Xft.dpi: 120.5\n"), Some(120.5));
        assert_eq!(parse_xft_dpi(b"Xcursor.size:\t24\n"), None);
        assert_eq!(parse_xft_dpi(b"Xft.dpi:\tlots\n"), None);
    }

    fn xsettings(le: bool) -> Vec<u8> {
        let mut v = Vec::new();
        let p16 =
            |v: &mut Vec<u8>, x: u16| v.extend(if le { x.to_le_bytes() } else { x.to_be_bytes() });
        let p32 =
            |v: &mut Vec<u8>, x: u32| v.extend(if le { x.to_le_bytes() } else { x.to_be_bytes() });
        v.extend([if le { 0 } else { 1 }, 0, 0, 0]);
        p32(&mut v, 7); // serial
        p32(&mut v, 3); // entries
        // a string, a color and the integer we want
        let name = b"Net/ThemeName";
        v.extend([1, 0]);
        p16(&mut v, name.len() as u16);
        v.extend(name);
        v.extend([0, 0, 0]); // pad 13 -> 16
        p32(&mut v, 0);
        p32(&mut v, 5);
        v.extend(b"Yaru\0\0\0\0");
        let name = b"Gtk/Color";
        v.extend([2, 0]);
        p16(&mut v, name.len() as u16);
        v.extend(name);
        v.extend([0, 0, 0]);
        p32(&mut v, 0);
        v.extend([0; 8]);
        let name = b"Gdk/WindowScalingFactor";
        v.extend([0, 0]);
        p16(&mut v, name.len() as u16);
        v.extend(name);
        v.push(0); // pad 23 -> 24
        p32(&mut v, 0);
        p32(&mut v, 2);
        v
    }

    #[test]
    fn xsettings_window_scaling_factor() {
        for le in [true, false] {
            let v = xsettings(le);
            assert_eq!(parse_xsettings_int(&v, "Gdk/WindowScalingFactor"), Some(2));
            assert_eq!(parse_xsettings_int(&v, "Gdk/Nope"), None);
            // truncated data never panics
            for n in 0..v.len() {
                let _ = parse_xsettings_int(&v[..n], "Gdk/WindowScalingFactor");
            }
        }
        assert_eq!(parse_xsettings_int(&[7, 0, 0, 0], "x"), None);
    }
}
