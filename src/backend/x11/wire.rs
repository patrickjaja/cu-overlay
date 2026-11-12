//! The requests the X11 backend makes, behind a small trait so the backend's
//! bookkeeping (window lifetimes, opacity changes, buffer reuse, sync and
//! capture-ack ordering) is unit tested against a recording mock, and the
//! one real implementation over x11rb's pure-Rust connection.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ColormapAlloc, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, ImageFormat, PropMode, VisualClass, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::NAME;
use super::layout::{RawLayout, RawMonitor, XRect, parse_xft_dpi, parse_xsettings_int};

/// What the backend learns from the event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XEv {
    /// A [`Wire::sync`] marker came back: the server processed everything
    /// sent before it.
    Synced(u64),
    /// The server finished reading this buffer (MIT-SHM completion).
    ShmDone(u32),
    /// This window lost its contents (re-put its current buffer).
    Expose(u32),
    /// RandR or a root property the layout depends on changed.
    Layout,
}

/// The backend's view of the X server.
pub trait Wire {
    /// The descriptor to poll.
    fn fd(&self) -> RawFd;
    fn flush(&mut self) -> Result<(), String>;
    /// The next event already received, if any (never blocks).
    fn poll(&mut self) -> Result<Option<XEv>, String>;
    /// Query the layout inputs (blocking round trips).
    fn layout(&mut self) -> Result<RawLayout, String>;
    /// A mapped override-redirect ARGB window with an empty input shape, the
    /// overlay's names and pid, and `_NET_WM_WINDOW_OPACITY` 0.
    fn create_window(&mut self, r: XRect) -> Result<u32, String>;
    fn configure(&mut self, win: u32, r: XRect) -> Result<(), String>;
    /// `_NET_WM_WINDOW_OPACITY` (0 transparent .. `u32::MAX` opaque).
    fn set_opacity(&mut self, win: u32, value: u32) -> Result<(), String>;
    fn destroy_window(&mut self, win: u32) -> Result<(), String>;
    /// A `w` x `h` BGRA buffer the server can read (an MIT-SHM segment).
    fn alloc(&mut self, w: i32, h: i32) -> Result<u32, String>;
    fn pixels(&mut self, buf: u32) -> &mut [u8];
    fn free(&mut self, buf: u32);
    /// Copy the whole `w` x `h` buffer to the window's origin; an
    /// [`XEv::ShmDone`] follows once the server has read it.
    fn put(&mut self, win: u32, buf: u32, w: i32, h: i32) -> Result<(), String>;
    /// Ask for an [`XEv::Synced`] with `token` once everything sent before
    /// was processed.
    fn sync(&mut self, token: u64) -> Result<(), String>;
}

fn xerr(e: impl std::fmt::Display) -> String {
    format!("x11: {e}")
}

struct Atoms {
    opacity: u32,
    wm_type: u32,
    wm_type_notification: u32,
    net_wm_name: u32,
    utf8: u32,
    pid: u32,
    bypass: u32,
    sync: u32,
    desktop_geometry: u32,
    workareas: u32,
    xsettings_sel: u32,
    xsettings: u32,
    cm_sel: u32,
}

struct Seg {
    ptr: *mut u8,
    len: usize,
}

/// The real X connection.
pub struct XConn {
    conn: RustConnection,
    root: u32,
    visual: u32,
    colormap: u32,
    gc: u32,
    sync_win: u32,
    atoms: Atoms,
    segs: HashMap<u32, Seg>,
    /// The XSETTINGS owner we watch for property changes.
    xsettings_owner: u32,
    /// X errors seen (logged with `CU_OVERLAY_DEBUG`).
    pub errors: u64,
    debug: bool,
}

/// What [`XConn::connect`] found out about the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerInfo {
    /// The `XWAYLAND` extension is present (the server is XWayland).
    pub xwayland: bool,
    /// A compositing manager owns `_NET_WM_CM_S<n>`.
    pub compositor: bool,
}

impl XConn {
    /// Connect to `$DISPLAY` and set up what every window needs (32-bit ARGB
    /// visual, colormap, GC, the sync window). `Err` when there is no
    /// display or it lacks a 32-bit visual, MIT-SHM, SHAPE or RandR 1.5.
    pub fn connect() -> Result<(Self, ServerInfo), String> {
        let (conn, screen_num) =
            RustConnection::connect(None).map_err(|e| format!("no X display: {e}"))?;
        let screen = conn.setup().roots[screen_num].clone();
        let root = screen.root;
        let ext = |name: &str| -> Result<bool, String> {
            Ok(conn
                .query_extension(name.as_bytes())
                .map_err(xerr)?
                .reply()
                .map_err(xerr)?
                .present)
        };
        for need in ["MIT-SHM", "SHAPE", "RANDR"] {
            if !ext(need)? {
                return Err(format!("the X server has no {need} extension"));
            }
        }
        let xwayland = ext("XWAYLAND")?;
        let rr = conn
            .randr_query_version(1, 5)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
        if (rr.major_version, rr.minor_version) < (1, 5) {
            return Err(format!(
                "the X server has RandR {}.{}, monitors need 1.5",
                rr.major_version, rr.minor_version
            ));
        }
        conn.shm_query_version()
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
        let visual = screen
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.class == VisualClass::TRUE_COLOR)
            .map(|v| v.visual_id)
            .ok_or("the X server has no 32-bit TrueColor visual")?;
        let intern = |name: &str| -> Result<u32, String> {
            Ok(conn
                .intern_atom(false, name.as_bytes())
                .map_err(xerr)?
                .reply()
                .map_err(xerr)?
                .atom)
        };
        let atoms = Atoms {
            opacity: intern("_NET_WM_WINDOW_OPACITY")?,
            wm_type: intern("_NET_WM_WINDOW_TYPE")?,
            wm_type_notification: intern("_NET_WM_WINDOW_TYPE_NOTIFICATION")?,
            net_wm_name: intern("_NET_WM_NAME")?,
            utf8: intern("UTF8_STRING")?,
            pid: intern("_NET_WM_PID")?,
            bypass: intern("_NET_WM_BYPASS_COMPOSITOR")?,
            sync: intern("_CLAUDE_CU_OVERLAY_SYNC")?,
            desktop_geometry: intern("_NET_DESKTOP_GEOMETRY")?,
            workareas: intern("_GTK_WORKAREAS_D0")?,
            xsettings_sel: intern(&format!("_XSETTINGS_S{screen_num}"))?,
            xsettings: intern("_XSETTINGS_SETTINGS")?,
            cm_sel: intern(&format!("_NET_WM_CM_S{screen_num}"))?,
        };
        let compositor = conn
            .get_selection_owner(atoms.cm_sel)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?
            .owner
            != x11rb::NONE;
        let colormap = conn.generate_id().map_err(xerr)?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, root, visual)
            .map_err(xerr)?;
        // a GC for depth-32 drawables (created on a scratch pixmap)
        let pix = conn.generate_id().map_err(xerr)?;
        conn.create_pixmap(32, pix, root, 1, 1).map_err(xerr)?;
        let gc = conn.generate_id().map_err(xerr)?;
        conn.create_gc(gc, pix, &CreateGCAux::new().graphics_exposures(0))
            .map_err(xerr)?;
        conn.free_pixmap(pix).map_err(xerr)?;
        // sync markers are ClientMessages sent to this window with an empty
        // event mask, which the server delivers to the window's creator
        let sync_win = conn.generate_id().map_err(xerr)?;
        conn.create_window(
            0,
            sync_win,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .map_err(xerr)?;
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(xerr)?;
        conn.randr_select_input(
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE,
        )
        .map_err(xerr)?;
        conn.flush().map_err(xerr)?;
        let debug = std::env::var_os("CU_OVERLAY_DEBUG").is_some_and(|v| v != "0");
        Ok((
            Self {
                conn,
                root,
                visual,
                colormap,
                gc,
                sync_win,
                atoms,
                segs: HashMap::new(),
                xsettings_owner: x11rb::NONE,
                errors: 0,
                debug,
            },
            ServerInfo {
                xwayland,
                compositor,
            },
        ))
    }

    fn prop(&self, win: u32, atom: u32, ty: u32, len: u32) -> Result<Option<Vec<u8>>, String> {
        let r = self
            .conn
            .get_property(false, win, atom, ty, 0, len)
            .map_err(xerr)?
            .reply();
        match r {
            Ok(r) if r.format != 0 => Ok(Some(r.value)),
            Ok(_) => Ok(None),
            // the XSETTINGS owner can vanish between the two requests
            Err(x11rb::errors::ReplyError::X11Error(_)) => Ok(None),
            Err(e) => Err(xerr(e)),
        }
    }

    fn hostname() -> Vec<u8> {
        std::fs::read("/proc/sys/kernel/hostname")
            .map(|mut h| {
                while h.last().is_some_and(|c| c.is_ascii_whitespace()) {
                    h.pop();
                }
                h
            })
            .unwrap_or_default()
    }
}

impl Drop for XConn {
    fn drop(&mut self) {
        for (_, s) in self.segs.drain() {
            // SAFETY: unmapping a mapping this connection owns, exactly once.
            unsafe { libc::munmap(s.ptr.cast(), s.len) };
        }
    }
}

impl Wire for XConn {
    fn fd(&self) -> RawFd {
        self.conn.stream().as_raw_fd()
    }

    fn flush(&mut self) -> Result<(), String> {
        self.conn.flush().map_err(xerr)
    }

    fn poll(&mut self) -> Result<Option<XEv>, String> {
        loop {
            let Some(ev) = self.conn.poll_for_event().map_err(xerr)? else {
                return Ok(None);
            };
            let out = match ev {
                Event::ClientMessage(m)
                    if m.window == self.sync_win && m.type_ == self.atoms.sync =>
                {
                    let d = m.data.as_data32();
                    Some(XEv::Synced(u64::from(d[0]) | (u64::from(d[1]) << 32)))
                }
                Event::ShmCompletion(c) => Some(XEv::ShmDone(c.shmseg)),
                Event::Expose(e) if e.count == 0 => Some(XEv::Expose(e.window)),
                Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_) => Some(XEv::Layout),
                Event::PropertyNotify(p)
                    if (p.window == self.root
                        && [
                            u32::from(AtomEnum::RESOURCE_MANAGER),
                            self.atoms.desktop_geometry,
                            self.atoms.workareas,
                        ]
                        .contains(&p.atom))
                        || p.atom == self.atoms.xsettings =>
                {
                    Some(XEv::Layout)
                }
                Event::Error(e) => {
                    self.errors += 1;
                    if self.debug {
                        eprintln!("cu-overlay: x11 error {e:?}");
                    }
                    None
                }
                _ => None,
            };
            if out.is_some() {
                return Ok(out);
            }
        }
    }

    fn layout(&mut self) -> Result<RawLayout, String> {
        let mons = self
            .conn
            .randr_get_monitors(self.root, true)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
        let mut monitors = Vec::new();
        for m in &mons.monitors {
            let name = self
                .conn
                .get_atom_name(m.name)
                .map_err(xerr)?
                .reply()
                .map_err(xerr)?
                .name;
            monitors.push(RawMonitor {
                name: String::from_utf8_lossy(&name).into_owned(),
                rect: XRect {
                    x: m.x.into(),
                    y: m.y.into(),
                    w: m.width.into(),
                    h: m.height.into(),
                },
            });
        }
        let g = self
            .conn
            .get_geometry(self.root)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?;
        let desktop = self
            .prop(
                self.root,
                self.atoms.desktop_geometry,
                AtomEnum::CARDINAL.into(),
                2,
            )?
            .filter(|v| v.len() >= 8)
            .map(|v| {
                let w = u32::from_ne_bytes(v[0..4].try_into().expect("4 bytes"));
                let h = u32::from_ne_bytes(v[4..8].try_into().expect("4 bytes"));
                (w as i32, h as i32)
            });
        let workareas = self
            .prop(
                self.root,
                self.atoms.workareas,
                AtomEnum::CARDINAL.into(),
                4 * 64,
            )?
            .map(|v| {
                v.as_chunks::<16>()
                    .0
                    .iter()
                    .map(|c| {
                        let n = |i: usize| {
                            u32::from_ne_bytes(c[i..i + 4].try_into().expect("4 bytes")) as i32
                        };
                        XRect {
                            x: n(0),
                            y: n(4),
                            w: n(8),
                            h: n(12),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let xft_dpi = self
            .prop(
                self.root,
                AtomEnum::RESOURCE_MANAGER.into(),
                AtomEnum::STRING.into(),
                1 << 16,
            )?
            .and_then(|v| parse_xft_dpi(&v));
        let owner = self
            .conn
            .get_selection_owner(self.atoms.xsettings_sel)
            .map_err(xerr)?
            .reply()
            .map_err(xerr)?
            .owner;
        let mut window_scaling = None;
        if owner != x11rb::NONE {
            if owner != self.xsettings_owner {
                // follow changes of the settings themselves; errors (owner
                // gone) arrive as events and are ignored
                let _ = self.conn.change_window_attributes(
                    owner,
                    &xproto::ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE),
                );
                self.xsettings_owner = owner;
            }
            window_scaling = self
                .prop(owner, self.atoms.xsettings, AtomEnum::ANY.into(), 1 << 16)?
                .and_then(|v| parse_xsettings_int(&v, "Gdk/WindowScalingFactor"));
        }
        Ok(RawLayout {
            monitors,
            root: (g.width.into(), g.height.into()),
            desktop,
            window_scaling,
            xft_dpi,
            workareas,
        })
    }

    fn create_window(&mut self, r: XRect) -> Result<u32, String> {
        let c = &self.conn;
        let id = c.generate_id().map_err(xerr)?;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(self.colormap)
            .override_redirect(1)
            .event_mask(EventMask::EXPOSURE);
        c.create_window(
            32,
            id,
            self.root,
            r.x as i16,
            r.y as i16,
            r.w.max(1) as u16,
            r.h.max(1) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &aux,
        )
        .map_err(xerr)?;
        // click-, hover- and scroll-through: no input region at all
        c.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            xproto::ClipOrdering::UNSORTED,
            id,
            0,
            0,
            &[],
        )
        .map_err(xerr)?;
        let name = NAME.as_bytes();
        c.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            name,
        )
        .map_err(xerr)?;
        c.change_property8(
            PropMode::REPLACE,
            id,
            self.atoms.net_wm_name,
            self.atoms.utf8,
            name,
        )
        .map_err(xerr)?;
        let class = [name, b"\0", name, b"\0"].concat();
        c.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            &class,
        )
        .map_err(xerr)?;
        c.change_property32(
            PropMode::REPLACE,
            id,
            self.atoms.pid,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        )
        .map_err(xerr)?;
        c.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLIENT_MACHINE,
            AtomEnum::STRING,
            &Self::hostname(),
        )
        .map_err(xerr)?;
        c.change_property32(
            PropMode::REPLACE,
            id,
            self.atoms.wm_type,
            AtomEnum::ATOM,
            &[self.atoms.wm_type_notification],
        )
        .map_err(xerr)?;
        // 2 = never unredirect this window
        c.change_property32(
            PropMode::REPLACE,
            id,
            self.atoms.bypass,
            AtomEnum::CARDINAL,
            &[2],
        )
        .map_err(xerr)?;
        c.change_property32(
            PropMode::REPLACE,
            id,
            self.atoms.opacity,
            AtomEnum::CARDINAL,
            &[0],
        )
        .map_err(xerr)?;
        c.map_window(id).map_err(xerr)?;
        Ok(id)
    }

    fn configure(&mut self, win: u32, r: XRect) -> Result<(), String> {
        self.conn
            .configure_window(
                win,
                &xproto::ConfigureWindowAux::new()
                    .x(r.x)
                    .y(r.y)
                    .width(r.w.max(1) as u32)
                    .height(r.h.max(1) as u32),
            )
            .map_err(xerr)?;
        Ok(())
    }

    fn set_opacity(&mut self, win: u32, value: u32) -> Result<(), String> {
        self.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                self.atoms.opacity,
                AtomEnum::CARDINAL,
                &[value],
            )
            .map_err(xerr)?;
        Ok(())
    }

    fn destroy_window(&mut self, win: u32) -> Result<(), String> {
        self.conn.destroy_window(win).map_err(xerr)?;
        Ok(())
    }

    fn alloc(&mut self, w: i32, h: i32) -> Result<u32, String> {
        let len = (w.max(1) as usize) * (h.max(1) as usize) * 4;
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
        let seg = self.conn.generate_id().map_err(xerr)?;
        self.segs.insert(
            seg,
            Seg {
                ptr: ptr.cast(),
                len,
            },
        );
        // the server only reads the segment
        self.conn.shm_attach_fd(seg, fd, true).map_err(xerr)?;
        Ok(seg)
    }

    fn pixels(&mut self, buf: u32) -> &mut [u8] {
        match self.segs.get(&buf) {
            // SAFETY: ptr/len is a live MAP_SHARED mapping owned by self.
            Some(s) => unsafe { std::slice::from_raw_parts_mut(s.ptr, s.len) },
            None => &mut [],
        }
    }

    fn free(&mut self, buf: u32) {
        if let Some(s) = self.segs.remove(&buf) {
            let _ = self.conn.shm_detach(buf);
            // SAFETY: unmapping a mapping we own; the server has its own.
            unsafe { libc::munmap(s.ptr.cast(), s.len) };
        }
    }

    fn put(&mut self, win: u32, buf: u32, w: i32, h: i32) -> Result<(), String> {
        let (w, h) = (w.max(1) as u16, h.max(1) as u16);
        self.conn
            .shm_put_image(
                win,
                self.gc,
                w,
                h,
                0,
                0,
                w,
                h,
                0,
                0,
                32,
                ImageFormat::Z_PIXMAP.into(),
                true,
                buf,
                0,
            )
            .map_err(xerr)?;
        Ok(())
    }

    fn sync(&mut self, token: u64) -> Result<(), String> {
        let ev = ClientMessageEvent::new(
            32,
            self.sync_win,
            self.atoms.sync,
            [token as u32, (token >> 32) as u32, 0, 0, 0],
        );
        self.conn
            .send_event(false, self.sync_win, EventMask::NO_EVENT, ev)
            .map_err(xerr)?;
        Ok(())
    }
}
