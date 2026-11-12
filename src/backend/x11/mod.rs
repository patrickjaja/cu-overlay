//! XWayland backend for compositors without wlr-layer-shell (GNOME/Mutter):
//! four override-redirect windows with a 32-bit ARGB visual and an empty
//! input shape on the shown output, stacked bottom to top: the glow or rim
//! (the whole output), the badge, the press ring and the arrow. Override-
//! redirect windows are never focused, not in Alt-Tab and on every
//! workspace; Mutter stacks them above normal and fullscreen windows and the
//! top bar.
//!
//! Opacity is `_NET_WM_WINDOW_OPACITY` (fades, the glow pulse, capture
//! hides, a cursor that is not drawn): a property change needs no redraw,
//! and on Mutter it reaches the screen far sooner than an unmap or map
//! (measured 35 ms median to a ScreenCast frame vs 140 ms for a map). The
//! output-sized window is drawn once per look and scale; only the small
//! windows are repainted while something moves, through MIT-SHM, and a
//! buffer is reused only after the server's completion event.
//!
//! Meant for XWayland on Wayland compositors without layer-shell. It is
//! picked automatically only when the server is XWayland and a compositing
//! manager runs (ARGB windows need one); on a real X11 session upstream's
//! own Electron overlay does the job. `CU_OVERLAY_BACKEND=x11` forces it
//! (tests on Xvfb).

pub mod layout;
mod wire;

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use super::{Backend, BackendEvent, Caps};
use crate::badge;
use crate::controller::Scene;
use crate::geom::{self, OutputInfo};
use crate::protocol::Look;
use crate::render::{self, ArrowParams, CURSOR_HOT, CURSOR_SIZE, RING_HALF, RING_SIZE};
use layout::{Layout, XRect};
pub use wire::{ServerInfo, Wire, XConn, XEv};

/// `WM_NAME`, `_NET_WM_NAME` and both `WM_CLASS` strings of every overlay
/// window (bridges recognise their own overlay by it and `_NET_WM_PID`).
pub const NAME: &str = "claude-cu-overlay";

/// Force the factor from X pixels to logical pixels (an integer >= 1),
/// for a caller that knows the compositor's layout (see
/// [`layout::compute`]).
pub const SCALE_ENV: &str = "CU_OVERLAY_X11_SCALE";

/// Glow pulse interval in ms (unset or 0: static glow, see
/// [`DEFAULT_PULSE`]).
pub const PULSE_ENV: &str = "CU_OVERLAY_X11_PULSE_MS";

/// After a present that made something invisible, a sync is reported only
/// this long after its round trip: the round trip proves XWayland processed
/// the opacity change, not that Mutter repainted. Measured on Mutter 48.1
/// (software rendering, 60 Hz): opacity 0 to the first clean ScreenCast
/// frame 32-44 ms (median 35); see DESIGN.md.
pub const HIDE_SETTLE: Duration = Duration::from_millis(50);

/// Frame pacing for animations: X has no frame callbacks for override-
/// redirect windows (Mutter sends no `_NET_WM_FRAME_DRAWN` to them), so a
/// frame is due this long after the last one, once its requests came back.
pub const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// The glow is static by default (upstream's base keyframe, its reduced-
/// motion look): every pulse step changes the output-sized window's
/// opacity, which makes Mutter repaint the whole output. Measured on the
/// software-rendered GNOME 48 VM: gnome-shell at 0.9 % of a core with the
/// static glow, 187 % with layer-shell's 30 Hz pulse, 200 % at 10 Hz and
/// still 156 % at 5 Hz. [`Options::pulse`] / `CU_OVERLAY_X11_PULSE_MS`
/// turn it on (100 ms is a sensible rate on GPU-composited desktops).
pub const DEFAULT_PULSE: Option<Duration> = None;

/// RandR and root property changes are read this long after the last one:
/// after a live scale change Mutter moves `_NET_DESKTOP_GEOMETRY` first and
/// gnome-settings-daemon updates XSETTINGS and `Xft.dpi` about 40 ms later
/// (measured), and the layout in between is wrong.
pub const LAYOUT_SETTLE: Duration = Duration::from_millis(250);

/// Internal sync tokens (frame pacing) have this bit set; the run loop's
/// tokens count up from 1.
const INTERNAL: u64 = 1 << 63;

/// Options for [`X11::connect`].
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Skip the "is XWayland" and "compositing manager runs" checks (an
    /// explicit `CU_OVERLAY_BACKEND=x11`).
    pub forced: bool,
    /// The X-to-logical factor, overriding detection.
    pub scale: Option<i32>,
    /// Glow pulse interval; `None` or zero: static glow ([`DEFAULT_PULSE`]).
    pub pulse: Option<Duration>,
}

impl Options {
    /// `forced` plus `CU_OVERLAY_X11_SCALE` and `CU_OVERLAY_X11_PULSE_MS`.
    pub fn from_env(forced: bool) -> Self {
        let num = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
        };
        Self {
            forced,
            scale: num(SCALE_ENV)
                .map(|v| v as i32)
                .filter(|v| (1..=layout::MAX_SCALE).contains(v)),
            pulse: num(PULSE_ENV).map(|ms| Duration::from_millis(ms.into())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Main,
    Badge,
    Ring,
    Cursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Buf {
    id: u32,
    w: i32,
    h: i32,
    busy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainKey {
    Rim,
    GlowPeak,
    GlowBase,
}

/// One overlay window and what is on it.
struct Win {
    id: u32,
    rect: XRect,
    /// Content key of the current buffer (`[i32; 8]` like layer-shell's).
    key: Option<[i32; 8]>,
    opacity: Option<u32>,
    bufs: Vec<Buf>,
    /// The buffer last put (re-put on Expose).
    cur: Option<u32>,
}

impl Win {
    fn new(id: u32, rect: XRect) -> Self {
        Self {
            id,
            rect,
            key: None,
            opacity: Some(0),
            bufs: Vec::new(),
            cur: None,
        }
    }
}

struct Surf {
    output: String,
    main: Win,
    badge: Win,
    ring: Win,
    cur: Win,
    /// A frame marker is out (`Some(token)`), and whether it came back.
    frame: Option<(u64, bool)>,
    frame_due: Instant,
}

impl Surf {
    fn win(&mut self, r: Role) -> &mut Win {
        match r {
            Role::Main => &mut self.main,
            Role::Badge => &mut self.badge,
            Role::Ring => &mut self.ring,
            Role::Cursor => &mut self.cur,
        }
    }
}

const ROLES: [Role; 4] = [Role::Main, Role::Badge, Role::Ring, Role::Cursor];

/// The X11 backend over a [`Wire`] ([`XConn`] in production).
pub struct X11<W: Wire = XConn> {
    w: W,
    forced_scale: Option<i32>,
    pulse: Option<Duration>,
    layout: Layout,
    /// The RandR correction last logged to stderr.
    logged_stale: Option<layout::Correction>,
    surf: Option<Surf>,
    events: Vec<BackendEvent>,
    /// A present since the last sync made something visible invisible.
    hid: bool,
    /// Sync tokens out, with whether they wait for [`HIDE_SETTLE`].
    out: HashMap<u64, bool>,
    /// Synced tokens held back until their settle time.
    due: Vec<(Instant, u64)>,
    next_internal: u64,
    settle: Duration,
    /// A layout re-read is due (debounced, see [`LAYOUT_SETTLE`]).
    relayout_at: Option<Instant>,
    layout_settle: Duration,
    fatal: Option<String>,
    /// Puts of the main, ring, arrow and badge windows, opacity changes.
    stats: [u64; 5],
}

fn mult(v: f64) -> u32 {
    (v.clamp(0.0, 1.0) * u32::MAX as f64).round() as u32
}

impl X11<XConn> {
    /// Connect to `$DISPLAY`. Unless `opts.forced`, refuse a server that is
    /// not XWayland (a real X11 session has upstream's own overlay) or has
    /// no compositing manager (ARGB windows would be opaque).
    pub fn connect(opts: Options) -> Result<Self, String> {
        let (w, info) = XConn::connect()?;
        if !opts.forced {
            if !info.xwayland {
                return Err("the X server is not XWayland (on a real X11 session upstream's own overlay runs; CU_OVERLAY_BACKEND=x11 forces it)".into());
            }
            if !info.compositor {
                return Err(
                    "no compositing manager owns _NET_WM_CM_S0 (ARGB windows need one)".into(),
                );
            }
        } else if !info.compositor {
            eprintln!(
                "cu-overlay: x11: no compositing manager, transparency will not show (forced)"
            );
        }
        Self::with_wire(w, opts)
    }
}

impl<W: Wire> X11<W> {
    /// The backend over any [`Wire`]: reads the layout once.
    pub fn with_wire(mut w: W, opts: Options) -> Result<Self, String> {
        let raw = w.layout()?;
        let mut be = Self {
            w,
            forced_scale: opts.scale,
            pulse: opts.pulse,
            layout: layout::compute(&raw, opts.scale),
            logged_stale: None,
            surf: None,
            events: Vec::new(),
            hid: false,
            out: HashMap::new(),
            due: Vec::new(),
            next_internal: 0,
            settle: HIDE_SETTLE,
            relayout_at: None,
            layout_settle: LAYOUT_SETTLE,
            fatal: None,
            stats: [0; 5],
        };
        be.log_stale();
        Ok(be)
    }

    fn log_stale(&mut self) {
        if self.layout.corrected != self.logged_stale {
            self.logged_stale = self.layout.corrected;
            match self.layout.corrected {
                Some(layout::Correction::WorkAreas) => eprintln!(
                    "cu-overlay: x11: XWayland's RandR is behind a scale change; monitor geometry is fitted to Mutter's work areas"
                ),
                Some(layout::Correction::Ratio(a, b)) => eprintln!(
                    "cu-overlay: x11: XWayland's RandR is behind a scale change; its geometry is scaled by {a}/{b}"
                ),
                None => {}
            }
        }
    }

    fn fail(&mut self, e: String) {
        self.fatal.get_or_insert(e);
    }

    fn set_opacity(&mut self, role: Role, v: u32) {
        let Some(s) = self.surf.as_mut() else { return };
        let win = s.win(role);
        if win.opacity == Some(v) {
            return;
        }
        if v == 0 && win.opacity.is_some_and(|o| o > 0) {
            self.hid = true;
        }
        win.opacity = Some(v);
        let id = win.id;
        self.stats[4] += 1;
        if let Err(e) = self.w.set_opacity(id, v) {
            self.fail(e);
        }
    }

    /// Move/resize a window when its rectangle changed.
    fn place(&mut self, role: Role, r: XRect) {
        let Some(s) = self.surf.as_mut() else { return };
        let win = s.win(role);
        if win.rect == r {
            return;
        }
        win.rect = r;
        let id = win.id;
        if let Err(e) = self.w.configure(id, r) {
            self.fail(e);
        }
    }

    /// Paint a new `w` x `h` buffer for `role` with `paint` and put it.
    fn draw(&mut self, role: Role, w: i32, h: i32, paint: &mut dyn FnMut(&mut [u8], usize, usize)) {
        let Some(s) = self.surf.as_mut() else { return };
        let win = s.win(role);
        let free = win
            .bufs
            .iter()
            .position(|b| !b.busy && b.w == w && b.h == h);
        let idx = match free {
            Some(i) => i,
            None => {
                // free buffers at a stale size go
                let stale: Vec<u32> = win
                    .bufs
                    .iter()
                    .filter(|b| !b.busy && Some(b.id) != win.cur)
                    .map(|b| b.id)
                    .collect();
                win.bufs.retain(|b| !stale.contains(&b.id));
                for id in stale {
                    self.w.free(id);
                }
                let id = match self.w.alloc(w, h) {
                    Ok(id) => id,
                    Err(e) => return self.fail(e),
                };
                let win = self.surf.as_mut().expect("surface checked").win(role);
                win.bufs.push(Buf {
                    id,
                    w,
                    h,
                    busy: false,
                });
                win.bufs.len() - 1
            }
        };
        let win = self.surf.as_mut().expect("surface checked").win(role);
        let b = &mut win.bufs[idx];
        b.busy = true;
        let (id, wid) = (b.id, win.id);
        win.cur = Some(id);
        let px = self.w.pixels(id);
        paint(px, w as usize, h as usize);
        self.stats[match role {
            Role::Main => 0,
            Role::Ring => 1,
            Role::Cursor => 2,
            Role::Badge => 3,
        }] += 1;
        if let Err(e) = self.w.put(wid, id, w, h) {
            self.fail(e);
        }
    }

    fn present_main(&mut self, scene: &Scene, out: &layout::XOutput) {
        let s = self.layout.scale;
        let key = match (scene.look, scene.pulse) {
            (Look::Rim, _) => MainKey::Rim,
            (Look::Glow, Some(_)) => MainKey::GlowPeak,
            (Look::Glow, None) => MainKey::GlowBase,
        };
        let (w, h) = (out.rect.w, out.rect.h);
        let k = [key as i32, w, h, s, 0, 0, 0, 0];
        self.place(Role::Main, out.rect);
        let surf = self.surf.as_mut().expect("surface checked");
        if surf.main.key != Some(k) {
            surf.main.key = Some(k);
            let scale = s as f32;
            self.draw(Role::Main, w, h, &mut |px, w, h| match key {
                MainKey::Rim => render::draw_rim(px, w, h, scale),
                MainKey::GlowPeak => render::draw_glow(px, w, h, scale, &render::GLOW_PEAK),
                MainKey::GlowBase => render::draw_glow(px, w, h, scale, &render::GLOW_BASE),
            });
        }
        let m = match scene.look {
            Look::Glow => scene.opacity * scene.pulse.unwrap_or(1.0),
            Look::Rim => scene.opacity,
        };
        self.set_opacity(Role::Main, mult(m));
    }

    /// A ring or arrow window: `draw` is `None` when not drawn, else the
    /// output-local logical anchor point, the anchor's offset in the full
    /// box, a content key and the painter `(buf, w, h, scale, ax, ay)`.
    #[allow(clippy::type_complexity)]
    fn present_sub(
        &mut self,
        role: Role,
        out: &layout::XOutput,
        opacity: f64,
        draw: Option<(
            (f64, f64),
            i32,
            [i32; 4],
            &dyn Fn(&mut [u8], usize, usize, f32, f32, f32),
        )>,
    ) {
        let s = self.layout.scale;
        let size = if role == Role::Ring {
            RING_SIZE
        } else {
            CURSOR_SIZE
        };
        let osize = (out.info.width, out.info.height);
        let placed = draw
            .filter(|_| opacity > 0.0)
            .and_then(|(local, anchor, k, paint)| {
                geom::sub_box(local, anchor, size, osize).map(|b| (b, k, paint))
            });
        let Some((bx, k, paint)) = placed else {
            return self.set_opacity(role, 0);
        };
        let r = XRect {
            x: out.rect.x + bx.x * s,
            y: out.rect.y + bx.y * s,
            w: bx.w * s,
            h: bx.h * s,
        };
        let q = |v: f64| (v * 32.0).round() as i32;
        let key = [
            q(bx.anchor_x),
            q(bx.anchor_y),
            k[0],
            k[1],
            k[2],
            k[3],
            (bx.w << 16) | bx.h,
            s,
        ];
        // the window has its new size before the image is put into it
        self.place(role, r);
        let surf = self.surf.as_mut().expect("surface checked");
        if surf.win(role).key != Some(key) {
            surf.win(role).key = Some(key);
            let (ax, ay) = (q(bx.anchor_x) as f32 / 32.0, q(bx.anchor_y) as f32 / 32.0);
            self.draw(role, r.w, r.h, &mut |px, w, h| {
                paint(px, w, h, s as f32, ax, ay)
            });
        }
        self.set_opacity(role, mult(opacity));
    }

    fn present_badge(&mut self, scene: &Scene, out: &layout::XOutput) {
        let s = self.layout.scale;
        let (ow, oh) = (out.info.width, out.info.height);
        let placed = scene.badge.as_ref().and_then(|b| {
            let layout = badge::Layout::new(&b.text);
            let (bw, bh) = layout.box_size();
            if bw > ow || bh > oh {
                return None;
            }
            let pose = badge::pose(b.ms, (f64::from(ow), f64::from(oh)), layout.w, layout.h)?;
            let opacity = pose.opacity * scene.opacity;
            (opacity > 0.0).then_some((b, layout, pose, opacity, (bw, bh)))
        });
        let Some((b, layout, pose, opacity, (bw, bh))) = placed else {
            return self.set_opacity(Role::Badge, 0);
        };
        let bx = ((pose.cx - f64::from(bw) / 2.0).round() as i32).clamp(0, ow - bw);
        let by = ((pose.cy - f64::from(bh) / 2.0).round() as i32).clamp(0, oh - bh);
        let scalef = s as f32;
        let (px, py) = (
            (pose.cx - f64::from(bx)) as f32 * scalef,
            (pose.cy - f64::from(by)) as f32 * scalef,
        );
        let (dot_alpha, dot_scale) = badge::dot(b.ms);
        let text_hash = b.text.bytes().fold(2_166_136_261u32, |h, c| {
            (h ^ u32::from(c)).wrapping_mul(16_777_619)
        }) as i32;
        let key = [
            (px * 16.0).round() as i32,
            (py * 16.0).round() as i32,
            (pose.scale * 1000.0).round() as i32,
            ((dot_alpha * 255.0).round() as i32) << 8 | (dot_scale * 255.0).round() as i32,
            text_hash,
            (bw << 16) | bh,
            s,
            0,
        ];
        let r = XRect {
            x: out.rect.x + bx * s,
            y: out.rect.y + by * s,
            w: bw * s,
            h: bh * s,
        };
        self.place(Role::Badge, r);
        let surf = self.surf.as_mut().expect("surface checked");
        if surf.badge.key != Some(key) {
            surf.badge.key = Some(key);
            let paint = badge::Paint2 {
                cx: px,
                cy: py,
                k: scalef * pose.scale as f32,
                dot_alpha: dot_alpha as f32,
                dot_scale: dot_scale as f32,
                opacity: 1.0,
            };
            self.draw(Role::Badge, r.w, r.h, &mut |buf, w, h| {
                badge::draw(buf, w, h, &layout, &paint)
            });
        }
        self.set_opacity(Role::Badge, mult(opacity));
    }

    fn present_inner(&mut self, scene: &Scene) {
        let Some(s) = &self.surf else { return };
        let Some(out) = self.layout.output(&s.output).cloned() else {
            return;
        };
        self.present_main(scene, &out);
        self.present_badge(scene, &out);

        let ring = scene.press.map(|p| {
            let f = p.frame;
            let key = [
                (f.ring_scale * 2000.0) as i32,
                (f.ring_alpha * 1000.0) as i32,
                (f.bloom_scale * 2000.0) as i32,
                (f.bloom_alpha * 1000.0) as i32,
            ];
            (out.info.to_local(p.x, p.y), f, key)
        });
        let ring_paint = ring.map(|(_, f, _)| {
            move |buf: &mut [u8], w: usize, h: usize, scale: f32, cx: f32, cy: f32| {
                render::draw_ring(buf, w, h, scale, cx, cy, &f, 1.0)
            }
        });
        self.present_sub(
            Role::Ring,
            &out,
            scene.opacity,
            ring.zip(ring_paint.as_ref()).map(|((pos, _, key), p)| {
                (
                    pos,
                    RING_HALF,
                    key,
                    p as &dyn Fn(&mut [u8], usize, usize, f32, f32, f32),
                )
            }),
        );

        let arrow = scene.arrow.map(|a| {
            let key = [
                (a.pose.deg * 50.0).round() as i32,
                (a.pose.lift * 500.0).round() as i32,
                (a.scale * 2000.0).round() as i32,
                0,
            ];
            (out.info.to_local(a.pose.x, a.pose.y), a, key)
        });
        let arrow_paint = arrow.map(|(_, a, _)| {
            move |buf: &mut [u8], w: usize, h: usize, scale: f32, hx: f32, hy: f32| {
                let p = ArrowParams {
                    hot_x: hx,
                    hot_y: hy,
                    deg: a.pose.deg as f32,
                    lift: a.pose.lift as f32,
                    press_scale: a.scale as f32,
                };
                render::draw_arrow(buf, w, h, scale, &p, 1.0)
            }
        });
        self.present_sub(
            Role::Cursor,
            &out,
            scene.opacity,
            arrow.zip(arrow_paint.as_ref()).map(|((pos, _, key), p)| {
                (
                    pos,
                    CURSOR_HOT,
                    key,
                    p as &dyn Fn(&mut [u8], usize, usize, f32, f32, f32),
                )
            }),
        );

        if scene.animating {
            let token = INTERNAL | self.next_internal;
            let s = self.surf.as_mut().expect("surface checked");
            if s.frame.is_none() {
                self.next_internal += 1;
                s.frame = Some((token, false));
                s.frame_due = Instant::now() + FRAME_INTERVAL;
                if let Err(e) = self.w.sync(token) {
                    self.fail(e);
                }
            }
        }
    }

    fn drop_surf(&mut self) {
        let Some(s) = self.surf.take() else { return };
        let wins = [s.main, s.badge, s.ring, s.cur];
        if wins.iter().any(|w| w.opacity.is_some_and(|o| o > 0)) {
            self.hid = true;
        }
        for w in wins {
            let _ = self.w.destroy_window(w.id);
            for b in w.bufs {
                self.w.free(b.id);
            }
        }
    }

    /// Re-read the layout after RandR or a root property changed.
    fn relayout(&mut self) -> Result<(), String> {
        let raw = self.w.layout()?;
        let new = layout::compute(&raw, self.forced_scale);
        if new == self.layout {
            return Ok(());
        }
        let old = std::mem::replace(&mut self.layout, new);
        self.log_stale();
        if old.infos() != self.layout.infos() {
            self.events.push(BackendEvent::OutputsChanged);
        }
        let Some(s) = &self.surf else { return Ok(()) };
        match self.layout.output(&s.output) {
            None => {
                self.drop_surf();
                self.events.push(BackendEvent::Closed(
                    "the output of the overlay was removed".into(),
                ));
            }
            Some(o) if old.output(&s.output) != Some(o) || old.scale != self.layout.scale => {
                // same output, new place or factor: redraw everything there
                let s = self.surf.as_mut().expect("surface checked");
                for r in ROLES {
                    s.win(r).key = None;
                }
                self.events.push(BackendEvent::Changed);
            }
            Some(_) => {}
        }
        Ok(())
    }

    fn handle(&mut self, ev: XEv) -> Result<(), String> {
        match ev {
            XEv::Synced(t) if t & INTERNAL != 0 => {
                if let Some(s) = self.surf.as_mut()
                    && let Some((tok, back)) = s.frame.as_mut()
                    && *tok == t
                {
                    *back = true;
                }
            }
            XEv::Synced(t) => match self.out.remove(&t) {
                Some(true) => self.due.push((Instant::now() + self.settle, t)),
                Some(false) => self.events.push(BackendEvent::Synced(t)),
                None => {}
            },
            XEv::ShmDone(id) => {
                if let Some(s) = self.surf.as_mut() {
                    for r in ROLES {
                        for b in s.win(r).bufs.iter_mut().filter(|b| b.id == id) {
                            b.busy = false;
                        }
                    }
                }
            }
            XEv::Expose(win) => {
                let Some(s) = self.surf.as_mut() else {
                    return Ok(());
                };
                for r in ROLES {
                    let w = s.win(r);
                    if w.id == win
                        && let Some(cur) = w.cur
                        && let Some(b) = w.bufs.iter().find(|b| b.id == cur)
                    {
                        let (id, bw, bh) = (b.id, b.w, b.h);
                        self.w.put(win, id, bw, bh)?;
                        break;
                    }
                }
            }
            XEv::Layout => self.relayout_at = Some(Instant::now() + self.layout_settle),
        }
        Ok(())
    }

    fn pump(&mut self) -> Result<(), String> {
        while let Some(ev) = self.w.poll()? {
            self.handle(ev)?;
        }
        let now = Instant::now();
        if self.relayout_at.is_some_and(|t| t <= now) {
            self.relayout_at = None;
            self.relayout()?;
        }
        let mut i = 0;
        while i < self.due.len() {
            if self.due[i].0 <= now {
                let (_, t) = self.due.remove(i);
                self.events.push(BackendEvent::Synced(t));
            } else {
                i += 1;
            }
        }
        if let Some(s) = self.surf.as_mut()
            && let Some((_, true)) = s.frame
            && s.frame_due <= now
        {
            s.frame = None;
            self.events.push(BackendEvent::Frame);
        }
        Ok(())
    }
}

impl<W: Wire> Drop for X11<W> {
    fn drop(&mut self) {
        if std::env::var_os("CU_OVERLAY_DEBUG").is_some_and(|v| v != "0") {
            let [m, r, c, b, o] = self.stats;
            eprintln!(
                "cu-overlay: x11 puts main={m} ring={r} arrow={c} badge={b}, opacity changes={o}"
            );
        }
    }
}

impl<W: Wire> Backend for X11<W> {
    fn caps(&self) -> Caps {
        Caps {
            name: "x11",
            alpha_modifier: true,
            fractional_scale: false,
            badge: true,
            pulse_interval: self.pulse.or(DEFAULT_PULSE).filter(|p| !p.is_zero()),
        }
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.layout.infos()
    }

    fn prepare(&mut self) -> Result<RawFd, String> {
        if let Some(e) = self.fatal.take() {
            return Err(e);
        }
        self.pump()?;
        self.w.flush()?;
        Ok(self.w.fd())
    }

    fn dispatch(&mut self, _readable: bool) -> Result<Vec<BackendEvent>, String> {
        if let Some(e) = self.fatal.take() {
            return Err(e);
        }
        self.pump()?;
        Ok(std::mem::take(&mut self.events))
    }

    fn next_wakeup(&self) -> Option<Instant> {
        if !self.events.is_empty() {
            return Some(Instant::now());
        }
        let mut t = self.due.iter().map(|d| d.0).chain(self.relayout_at).min();
        if let Some(s) = &self.surf
            && let Some((_, true)) = s.frame
        {
            t = Some(t.map_or(s.frame_due, |x| x.min(s.frame_due)));
        }
        t
    }

    fn create(&mut self, output: &str) -> Result<(), String> {
        self.destroy();
        let out = self
            .layout
            .output(output)
            .cloned()
            .ok_or_else(|| format!("unknown output: {output}"))?;
        // the small windows start at one logical pixel: under native scaling
        // Mutter never showed a window created at 1x1 X pixels (not a
        // multiple of the factor), not even after it was resized
        let s = self.layout.scale;
        let small = XRect {
            x: out.rect.x,
            y: out.rect.y,
            w: s,
            h: s,
        };
        // created (and so stacked) bottom to top
        let mut ids = Vec::new();
        for r in [out.rect, small, small, small] {
            match self.w.create_window(r) {
                Ok(id) => ids.push((id, r)),
                Err(e) => {
                    for (id, _) in ids {
                        let _ = self.w.destroy_window(id);
                    }
                    return Err(e);
                }
            }
        }
        let mut it = ids.into_iter().map(|(id, r)| Win::new(id, r));
        let mut next = || it.next().expect("four windows");
        self.surf = Some(Surf {
            output: output.to_string(),
            main: next(),
            badge: next(),
            ring: next(),
            cur: next(),
            frame: None,
            frame_due: Instant::now(),
        });
        self.events.push(BackendEvent::Configured);
        Ok(())
    }

    fn destroy(&mut self) {
        self.drop_surf();
    }

    fn ready_to_draw(&self) -> bool {
        self.surf.is_some()
    }

    fn current(&self) -> Option<(String, f64)> {
        let s = self.surf.as_ref()?;
        Some((s.output.clone(), self.layout.scale as f64))
    }

    fn present(&mut self, scene: &Scene) {
        self.present_inner(scene);
    }

    fn sync(&mut self, token: u64) {
        self.out.insert(token, std::mem::take(&mut self.hid));
        if let Err(e) = self.w.sync(token) {
            self.fail(e);
        }
    }
}

#[cfg(test)]
mod tests;
