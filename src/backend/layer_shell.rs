//! Wayland backend: a wlr-layer-shell surface on the overlay layer of one
//! output, anchored to all edges, exclusive zone -1, no keyboard
//! interactivity and an empty input region, so every click and key goes to
//! the windows below. Two synchronized subsurfaces carry the press ring and
//! the drawn arrow, so moving the arrow only redraws a small buffer and
//! buffer + position land atomically with the parent commit.
//!
//! Optional globals: wp_viewporter + wp_fractional_scale_v1 (crisp at
//! fractional scales), wp_alpha_modifier_v1 (fades, glow pulse and capture
//! hide without redrawing), zxdg_output_manager_v1 (logical output layout).

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
    wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1, wp_alpha_modifier_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::{Backend, BackendEvent, Caps};
use crate::controller::Scene;
use crate::geom::{self, OutputInfo};
use crate::protocol::Look;
use crate::render::{self, ArrowParams, CURSOR_HOT, CURSOR_SIZE, RING_HALF, RING_SIZE};

/// Layer-shell namespace of the overlay surface.
pub const NAMESPACE: &str = "claude-cu-overlay";

/// Comma-separated optional globals to ignore (`alpha-modifier`,
/// `fractional-scale`, `viewporter`), for testing the fallbacks.
pub const DISABLE_ENV: &str = "CU_OVERLAY_DISABLE";

struct Globals {
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    shm: wl_shm::WlShm,
    layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    alpha: Option<wp_alpha_modifier_v1::WpAlphaModifierV1>,
    xdg_output: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
}

#[derive(Default)]
struct Output {
    global: u32,
    wl: Option<wl_output::WlOutput>,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    name: Option<String>,
    geometry_pos: (i32, i32),
    swap: bool,
    mode: (i32, i32),
    int_scale: i32,
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

impl Output {
    fn info(&self) -> Option<OutputInfo> {
        let name = self.name.clone()?;
        let int_scale = self.int_scale.max(1);
        let (x, y) = self.logical_pos.unwrap_or(self.geometry_pos);
        let (width, height) = match self.logical_size {
            Some(s) => s,
            None => {
                let (mw, mh) = if self.swap {
                    (self.mode.1, self.mode.0)
                } else {
                    self.mode
                };
                (mw / int_scale, mh / int_scale)
            }
        };
        if width <= 0 || height <= 0 {
            return None;
        }
        let scale =
            geom::output_scale(self.mode, (width, height), self.swap).unwrap_or(int_scale as f64);
        Some(OutputInfo {
            name,
            x,
            y,
            width,
            height,
            scale,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Main,
    Ring,
    Cursor,
}

struct Buf {
    wl: wl_buffer::WlBuffer,
    ptr: *mut u8,
    len: usize,
    w: i32,
    h: i32,
    role: Role,
    busy: bool,
}

impl Buf {
    fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: ptr/len is a live MAP_SHARED mapping owned by this Buf.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        self.wl.destroy();
        // SAFETY: unmapping the mapping this Buf owns, exactly once.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainKey {
    Rim(i32, i32),
    GlowPeak(i32, i32),
    GlowBase(i32, i32),
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubKey([i32; 8]);

/// A subsurface (ring or arrow) and what is committed on it.
struct Sub {
    surf: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    vp: Option<wp_viewport::WpViewport>,
    alpha: Option<wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1>,
    key: Option<SubKey>,
    mult: Option<u32>,
    pos: Option<(i32, i32)>,
}

impl Sub {
    fn destroy(self) {
        if let Some(a) = self.alpha {
            a.destroy();
        }
        if let Some(v) = self.vp {
            v.destroy();
        }
        self.sub.destroy();
        self.surf.destroy();
    }
}

struct Surf {
    generation: u32,
    output: String,
    main: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    vp: Option<wp_viewport::WpViewport>,
    frac: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    alpha: Option<wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1>,
    ring: Sub,
    cur: Sub,
    configured: bool,
    size: (i32, i32),
    /// Preferred scale from wp_fractional_scale_v1, in 120ths.
    pref120: Option<u32>,
    frame_pending: bool,
    main_key: Option<(MainKey, u32)>,
    main_mult: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
enum Cb {
    Frame(u32),
    Sync(u64),
}

struct St {
    g: Globals,
    outputs: Vec<Output>,
    surf: Option<Surf>,
    generation: u32,
    bufs: HashMap<u32, Buf>,
    next_buf: u32,
    events: Vec<BackendEvent>,
}

pub struct LayerShell {
    conn: Connection,
    queue: EventQueue<St>,
    qh: QueueHandle<St>,
    st: St,
    guard: Option<wayland_client::backend::ReadEventsGuard>,
}

fn wl_err(e: impl std::fmt::Display) -> String {
    format!("wayland: {e}")
}

fn memfd(len: usize) -> Result<OwnedFd, String> {
    // SAFETY: memfd_create with a static C string.
    let fd = unsafe { libc::memfd_create(c"cu-overlay".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fd was just returned by memfd_create.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ftruncate on our own fd.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
        return Err(format!("ftruncate: {}", std::io::Error::last_os_error()));
    }
    Ok(fd)
}

impl St {
    fn info(&self, name: &str) -> Option<OutputInfo> {
        self.outputs
            .iter()
            .filter_map(Output::info)
            .find(|o| o.name == name)
    }

    /// A free buffer of `role` and size, allocating one if none is free.
    fn take_buf(
        &mut self,
        role: Role,
        w: i32,
        h: i32,
        qh: &QueueHandle<St>,
    ) -> Result<u32, String> {
        if let Some((&id, _)) = self
            .bufs
            .iter()
            .find(|(_, b)| b.role == role && !b.busy && b.w == w && b.h == h)
        {
            return Ok(id);
        }
        // free buffers of this role at a stale size go
        self.bufs
            .retain(|_, b| !(b.role == role && !b.busy && (b.w != w || b.h != h)));
        let len = (w as usize) * (h as usize) * 4;
        let fd = memfd(len)?;
        // SAFETY: mapping the memfd we just sized; checked for MAP_FAILED.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        let id = self.next_buf;
        self.next_buf = self.next_buf.wrapping_add(1);
        let pool = self.g.shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let wl = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, id);
        pool.destroy();
        self.bufs.insert(
            id,
            Buf {
                wl,
                ptr: ptr.cast(),
                len,
                w,
                h,
                role,
                busy: false,
            },
        );
        Ok(id)
    }

    fn drop_surf(&mut self) {
        if let Some(s) = self.surf.take() {
            if let Some(a) = s.alpha {
                a.destroy();
            }
            if let Some(v) = s.vp {
                v.destroy();
            }
            if let Some(f) = s.frac {
                f.destroy();
            }
            s.ring.destroy();
            s.cur.destroy();
            s.layer.destroy();
            s.main.destroy();
        }
        // every buffer belonged to the destroyed surfaces
        self.bufs.clear();
    }
}

fn mult(v: f64) -> u32 {
    (v.clamp(0.0, 1.0) * u32::MAX as f64).round() as u32
}

impl LayerShell {
    /// Connect to `$WAYLAND_DISPLAY`, bind the globals and learn the outputs.
    /// `Err` when there is no display or no layer-shell.
    pub fn connect() -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland display: {e}"))?;
        let (globals, mut queue) = registry_queue_init::<St>(&conn).map_err(wl_err)?;
        let qh = queue.handle();
        let disabled = std::env::var(DISABLE_ENV).unwrap_or_default();
        let off = |what: &str| disabled.split(',').any(|d| d.trim() == what);
        let layer_shell = globals
            .bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(&qh, 1..=4, ())
            .map_err(|_| {
                "the compositor has no wlr-layer-shell (zwlr_layer_shell_v1)".to_string()
            })?;
        let g = Globals {
            compositor: globals
                .bind(&qh, 4..=6, ())
                .map_err(|e| format!("wl_compositor >= 4: {e}"))?,
            subcompositor: globals
                .bind(&qh, 1..=1, ())
                .map_err(|e| format!("wl_subcompositor: {e}"))?,
            shm: globals
                .bind(&qh, 1..=1, ())
                .map_err(|e| format!("wl_shm: {e}"))?,
            layer_shell,
            viewporter: if off("viewporter") {
                None
            } else {
                globals.bind(&qh, 1..=1, ()).ok()
            },
            fractional: if off("fractional-scale") || off("viewporter") {
                None
            } else {
                globals.bind(&qh, 1..=1, ()).ok()
            },
            alpha: if off("alpha-modifier") {
                None
            } else {
                globals.bind(&qh, 1..=1, ()).ok()
            },
            xdg_output: globals.bind(&qh, 1..=3, ()).ok(),
        };
        let mut st = St {
            g,
            outputs: Vec::new(),
            surf: None,
            generation: 0,
            bufs: HashMap::new(),
            next_buf: 1,
            events: Vec::new(),
        };
        let registry = globals.registry().clone();
        globals.contents().with_list(|list| {
            for gl in list {
                if gl.interface == "wl_output" {
                    bind_output(&mut st, &registry, gl.name, gl.version, &qh);
                }
            }
        });
        // two round trips: wl_output / xdg_output events, then xdg_output's
        // done (sent after the xdg objects exist)
        queue.roundtrip(&mut st).map_err(wl_err)?;
        queue.roundtrip(&mut st).map_err(wl_err)?;
        st.events.clear();
        Ok(Self {
            conn,
            queue,
            qh,
            st,
            guard: None,
        })
    }

    fn scale120(&self) -> u32 {
        let Some(s) = &self.st.surf else { return 120 };
        if let Some(p) = s.pref120 {
            return p;
        }
        let out = self.st.info(&s.output).map_or(1.0, |o| o.scale);
        if self.st.g.viewporter.is_some() {
            geom::scale120(out)
        } else {
            120 * (out.ceil().max(1.0) as u32)
        }
    }

    fn present_main(&mut self, scene: &Scene, s120: u32) -> Result<bool, String> {
        let st = &mut self.st;
        let has_alpha = st.g.alpha.is_some();
        let Some(s) = st.surf.as_ref() else {
            return Ok(false);
        };
        let (lw, lh) = s.size;
        let (w, h) = (geom::to_px(lw, s120), geom::to_px(lh, s120));
        let key = if !has_alpha && scene.opacity <= 0.0 {
            MainKey::Clear
        } else {
            match (scene.look, scene.pulse) {
                (Look::Rim, _) => MainKey::Rim(w, h),
                (Look::Glow, Some(_)) => MainKey::GlowPeak(w, h),
                (Look::Glow, None) => MainKey::GlowBase(w, h),
            }
        };
        let mut changed = false;
        if s.main_key != Some((key, s120)) {
            let viewport = s.vp.is_some();
            let (bw, bh) = match key {
                MainKey::Clear if viewport => (1, 1),
                _ => (w, h),
            };
            let id = st.take_buf(Role::Main, bw, bh, &self.qh)?;
            let b = st.bufs.get_mut(&id).expect("buffer just taken");
            let scale = s120 as f32 / 120.0;
            let (uw, uh) = (bw as usize, bh as usize);
            match key {
                MainKey::Rim(..) => render::draw_rim(b.pixels(), uw, uh, scale),
                MainKey::GlowPeak(..) => {
                    render::draw_glow(b.pixels(), uw, uh, scale, &render::GLOW_PEAK)
                }
                MainKey::GlowBase(..) => {
                    render::draw_glow(b.pixels(), uw, uh, scale, &render::GLOW_BASE)
                }
                MainKey::Clear => render::clear(b.pixels()),
            }
            b.busy = true;
            let wl = b.wl.clone();
            let s = st.surf.as_mut().expect("surface checked above");
            s.main.attach(Some(&wl), 0, 0);
            s.main.damage_buffer(0, 0, bw, bh);
            match &s.vp {
                Some(vp) => vp.set_destination(lw, lh),
                None => s.main.set_buffer_scale((s120 / 120) as i32),
            }
            s.main_key = Some((key, s120));
            changed = true;
        }
        let s = st.surf.as_mut().expect("surface checked above");
        if let Some(a) = &s.alpha {
            let m = match scene.look {
                Look::Glow => scene.opacity * scene.pulse.unwrap_or(1.0),
                Look::Rim => scene.opacity,
            };
            let m = mult(m);
            if s.main_mult != Some(m) {
                a.set_multiplier(m);
                s.main_mult = Some(m);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Update one subsurface: `draw` is `None` to unmap it, else the logical
    /// local position of its anchor point, the anchor's offset inside the
    /// surface, the content key and a painter.
    #[allow(clippy::type_complexity)]
    fn present_sub(
        &mut self,
        ring: bool,
        s120: u32,
        opacity: f64,
        draw: Option<(
            (f64, f64),
            i32,
            [i32; 5],
            &dyn Fn(&mut [u8], usize, f32, f32, f32),
        )>,
    ) -> Result<bool, String> {
        let has_alpha = self.st.g.alpha.is_some();
        let (role, size) = if ring {
            (Role::Ring, RING_SIZE)
        } else {
            (Role::Cursor, CURSOR_SIZE)
        };
        let draw = draw.filter(|_| opacity > 0.0 || has_alpha);
        let st = &mut self.st;
        let Some(surf) = st.surf.as_mut() else {
            return Ok(false);
        };
        let sub = if ring { &mut surf.ring } else { &mut surf.cur };
        let Some(((lx, ly), anchor, k, paint)) = draw else {
            if sub.key.is_some() {
                sub.surf.attach(None, 0, 0);
                sub.surf.commit();
                sub.key = None;
                return Ok(true);
            }
            return Ok(false);
        };
        let (px, fx) = geom::place(lx, anchor);
        let (py, fy) = geom::place(ly, anchor);
        let baked = if has_alpha { 1.0 } else { opacity };
        let key = SubKey([
            (fx * 32.0).round() as i32,
            (fy * 32.0).round() as i32,
            k[0],
            k[1],
            k[2],
            k[3],
            k[4],
            ((baked * 255.0).round() as i32) << 12 | s120 as i32,
        ]);
        let mut changed = false;
        if sub.key != Some(key) {
            let pxs = geom::to_px(size, s120);
            let id = st.take_buf(role, pxs, pxs, &self.qh)?;
            let b = st.bufs.get_mut(&id).expect("buffer just taken");
            let scale = s120 as f32 / 120.0;
            paint(
                b.pixels(),
                pxs as usize,
                scale,
                anchor as f32 + (fx * 32.0).round() as f32 / 32.0,
                anchor as f32 + (fy * 32.0).round() as f32 / 32.0,
            );
            if baked < 1.0 {
                render::apply_opacity(b.pixels(), baked as f32);
            }
            b.busy = true;
            let wl = b.wl.clone();
            let surf = st.surf.as_mut().expect("surface checked above");
            let sub = if ring { &mut surf.ring } else { &mut surf.cur };
            sub.surf.attach(Some(&wl), 0, 0);
            sub.surf.damage_buffer(0, 0, pxs, pxs);
            match &sub.vp {
                Some(vp) => vp.set_destination(size, size),
                None => sub.surf.set_buffer_scale((s120 / 120) as i32),
            }
            sub.key = Some(key);
            changed = true;
        }
        let surf = st.surf.as_mut().expect("surface checked above");
        let sub = if ring { &mut surf.ring } else { &mut surf.cur };
        if let Some(a) = &sub.alpha {
            let m = mult(opacity);
            if sub.mult != Some(m) {
                a.set_multiplier(m);
                sub.mult = Some(m);
                changed = true;
            }
        }
        if changed {
            sub.surf.commit();
        }
        if sub.pos != Some((px, py)) {
            sub.sub.set_position(px, py);
            sub.pos = Some((px, py));
            changed = true;
        }
        Ok(changed)
    }

    fn present_inner(&mut self, scene: &Scene) -> Result<(), String> {
        let Some(s) = &self.st.surf else {
            return Ok(());
        };
        if !s.configured {
            return Ok(());
        }
        let Some(out) = self.st.info(&s.output) else {
            return Ok(());
        };
        let s120 = self.scale120();
        let mut changed = self.present_main(scene, s120)?;

        let ring = scene.press.map(|p| {
            let f = p.frame;
            let key = [
                (f.ring_scale * 2000.0) as i32,
                (f.ring_alpha * 1000.0) as i32,
                (f.bloom_scale * 2000.0) as i32,
                (f.bloom_alpha * 1000.0) as i32,
                0,
            ];
            (out.to_local(p.x, p.y), f, key)
        });
        let ring_paint = ring.map(|(_, f, _)| {
            move |buf: &mut [u8], n: usize, scale: f32, cx: f32, cy: f32| {
                render::draw_ring(buf, n, n, scale, cx, cy, &f, 1.0)
            }
        });
        changed |= self.present_sub(
            true,
            s120,
            scene.opacity,
            ring.zip(ring_paint.as_ref()).map(|((pos, _, key), p)| {
                (
                    pos,
                    RING_HALF,
                    key,
                    p as &dyn Fn(&mut [u8], usize, f32, f32, f32),
                )
            }),
        )?;

        let arrow = scene.arrow.map(|a| {
            let key = [
                (a.pose.deg * 50.0).round() as i32,
                (a.pose.lift * 500.0).round() as i32,
                (a.scale * 2000.0).round() as i32,
                0,
                0,
            ];
            (out.to_local(a.pose.x, a.pose.y), a, key)
        });
        let arrow_paint = arrow.map(|(_, a, _)| {
            move |buf: &mut [u8], n: usize, scale: f32, hx: f32, hy: f32| {
                let p = ArrowParams {
                    hot_x: hx,
                    hot_y: hy,
                    deg: a.pose.deg as f32,
                    lift: a.pose.lift as f32,
                    press_scale: a.scale as f32,
                };
                render::draw_arrow(buf, n, n, scale, &p, 1.0)
            }
        });
        changed |= self.present_sub(
            false,
            s120,
            scene.opacity,
            arrow.zip(arrow_paint.as_ref()).map(|((pos, _, key), p)| {
                (
                    pos,
                    CURSOR_HOT,
                    key,
                    p as &dyn Fn(&mut [u8], usize, f32, f32, f32),
                )
            }),
        )?;

        let s = self.st.surf.as_mut().expect("surface checked above");
        if scene.animating && !s.frame_pending {
            s.main.frame(&self.qh, Cb::Frame(s.generation));
            s.frame_pending = true;
            changed = true;
        }
        if changed {
            s.main.commit();
        }
        Ok(())
    }
}

fn bind_output(
    st: &mut St,
    registry: &wl_registry::WlRegistry,
    name: u32,
    version: u32,
    qh: &QueueHandle<St>,
) {
    let wl: wl_output::WlOutput = registry.bind(name, version.clamp(2, 4), qh, name);
    let xdg =
        st.g.xdg_output
            .as_ref()
            .map(|m| m.get_xdg_output(&wl, qh, name));
    st.outputs.push(Output {
        global: name,
        wl: Some(wl),
        xdg,
        int_scale: 1,
        ..Output::default()
    });
}

impl Backend for LayerShell {
    fn caps(&self) -> Caps {
        Caps {
            name: "layer-shell",
            alpha_modifier: self.st.g.alpha.is_some(),
            fractional_scale: self.st.g.fractional.is_some(),
        }
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.st.outputs.iter().filter_map(Output::info).collect()
    }

    fn prepare(&mut self) -> Result<RawFd, String> {
        loop {
            self.queue.dispatch_pending(&mut self.st).map_err(wl_err)?;
            match self.conn.flush() {
                Ok(()) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(wl_err(e)),
            }
            if let Some(g) = self.queue.prepare_read() {
                let fd = g.connection_fd().as_raw_fd();
                self.guard = Some(g);
                return Ok(fd);
            }
        }
    }

    fn dispatch(&mut self, readable: bool) -> Result<Vec<BackendEvent>, String> {
        if let Some(g) = self.guard.take()
            && readable
        {
            match g.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(wl_err(e)),
            }
        }
        self.queue.dispatch_pending(&mut self.st).map_err(wl_err)?;
        Ok(std::mem::take(&mut self.st.events))
    }

    fn create(&mut self, output: &str) -> Result<(), String> {
        self.destroy();
        let qh = &self.qh;
        let st = &mut self.st;
        let wl_out = st
            .outputs
            .iter()
            .find(|o| o.name.as_deref() == Some(output))
            .and_then(|o| o.wl.clone())
            .ok_or_else(|| format!("unknown output: {output}"))?;
        st.generation = st.generation.wrapping_add(1);
        let generation = st.generation;
        let g = &st.g;
        let empty = g.compositor.create_region(qh, ());
        let mk_sub = |parent: &wl_surface::WlSurface| {
            let surf = g.compositor.create_surface(qh, ());
            surf.set_input_region(Some(&empty));
            let sub = g.subcompositor.get_subsurface(&surf, parent, qh, ());
            Sub {
                vp: g.viewporter.as_ref().map(|v| v.get_viewport(&surf, qh, ())),
                alpha: g.alpha.as_ref().map(|a| a.get_surface(&surf, qh, ())),
                surf,
                sub,
                key: None,
                mult: None,
                pos: None,
            }
        };
        let main = g.compositor.create_surface(qh, ());
        main.set_input_region(Some(&empty));
        // created in stacking order: ring below the arrow, both above main
        let ring = mk_sub(&main);
        let cur = mk_sub(&main);
        empty.destroy();
        let layer = g.layer_shell.get_layer_surface(
            &main,
            Some(&wl_out),
            zwlr_layer_shell_v1::Layer::Overlay,
            NAMESPACE.into(),
            qh,
            generation,
        );
        use zwlr_layer_surface_v1::Anchor;
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        layer.set_size(0, 0);
        let vp = g.viewporter.as_ref().map(|v| v.get_viewport(&main, qh, ()));
        let frac = g
            .fractional
            .as_ref()
            .map(|f| f.get_fractional_scale(&main, qh, generation));
        let alpha = g.alpha.as_ref().map(|a| a.get_surface(&main, qh, ()));
        main.commit();
        st.surf = Some(Surf {
            generation,
            output: output.to_string(),
            main,
            layer,
            vp,
            frac,
            alpha,
            ring,
            cur,
            configured: false,
            size: (0, 0),
            pref120: None,
            frame_pending: false,
            main_key: None,
            main_mult: None,
        });
        Ok(())
    }

    fn destroy(&mut self) {
        self.st.drop_surf();
    }

    fn ready_to_draw(&self) -> bool {
        self.st.surf.as_ref().is_some_and(|s| s.configured)
    }

    fn current(&self) -> Option<(String, f64)> {
        let s = self.st.surf.as_ref()?;
        Some((s.output.clone(), self.scale120() as f64 / 120.0))
    }

    fn present(&mut self, scene: &Scene) {
        if let Err(e) = self.present_inner(scene) {
            self.st.events.push(BackendEvent::Closed(e));
            self.st.drop_surf();
        }
    }

    fn sync(&mut self, token: u64) {
        self.conn.display().sync(&self.qh, Cb::Sync(token));
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for St {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => bind_output(st, registry, name, version, qh),
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = st.outputs.iter().position(|o| o.global == name) {
                    let o = st.outputs.remove(i);
                    if let Some(x) = o.xdg {
                        x.destroy();
                    }
                    if let Some(w) = o.wl
                        && w.version() >= 3
                    {
                        w.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for St {
    fn event(
        st: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = st.outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                o.geometry_pos = (x, y);
                o.swap = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if let WEnum::Value(f) = flags
                    && f.contains(wl_output::Mode::Current)
                {
                    o.mode = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => o.int_scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Done => st.events.push(BackendEvent::Changed),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for St {
    fn event(
        st: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = st.outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } if o.name.is_none() => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, u32> for St {
    fn event(
        st: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(b) = st.bufs.get_mut(id)
        {
            b.busy = false;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, Cb> for St {
    fn event(
        st: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        cb: &Cb,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            match *cb {
                Cb::Frame(generation) => {
                    if let Some(s) = st.surf.as_mut()
                        && s.generation == generation
                    {
                        s.frame_pending = false;
                        st.events.push(BackendEvent::Frame);
                    }
                }
                Cb::Sync(token) => st.events.push(BackendEvent::Synced(token)),
            }
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for St {
    fn event(
        st: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        generation: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let current = st
            .surf
            .as_ref()
            .is_some_and(|s| s.generation == *generation);
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                if !current {
                    return;
                }
                layer.ack_configure(serial);
                let s = st.surf.as_mut().expect("current surface");
                let mut size = (width as i32, height as i32);
                if size.0 <= 0 || size.1 <= 0 {
                    let out = s.output.clone();
                    if let Some(o) = st.info(&out) {
                        size = (o.width, o.height);
                    }
                }
                let s = st.surf.as_mut().expect("current surface");
                s.size = size;
                if s.configured {
                    st.events.push(BackendEvent::Changed);
                } else {
                    s.configured = true;
                    st.events.push(BackendEvent::Configured);
                }
            }
            zwlr_layer_surface_v1::Event::Closed if current => {
                st.drop_surf();
                st.events.push(BackendEvent::Closed(
                    "the compositor closed the overlay surface".into(),
                ));
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for St {
    fn event(
        st: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        generation: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(s) = st.surf.as_mut()
            && s.generation == *generation
            && s.pref120 != Some(scale)
        {
            s.pref120 = Some(scale);
            st.events.push(BackendEvent::Changed);
        }
    }
}

delegate_noop!(St: ignore wl_compositor::WlCompositor);
delegate_noop!(St: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(St: ignore wl_subsurface::WlSubsurface);
delegate_noop!(St: ignore wl_region::WlRegion);
delegate_noop!(St: ignore wl_shm::WlShm);
delegate_noop!(St: ignore wl_shm_pool::WlShmPool);
delegate_noop!(St: ignore wl_surface::WlSurface);
delegate_noop!(St: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(St: ignore wp_viewporter::WpViewporter);
delegate_noop!(St: ignore wp_viewport::WpViewport);
delegate_noop!(St: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(St: ignore wp_alpha_modifier_v1::WpAlphaModifierV1);
delegate_noop!(St: ignore wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1);
delegate_noop!(St: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
