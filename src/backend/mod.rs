//! The seam between the run loop and a windowing system: the layer-shell
//! backend (`layer-shell` feature, Wayland compositors with
//! wlr-layer-shell) and the X11 backend (`x11` feature, XWayland on
//! compositors without it, i.e. GNOME/Mutter).

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use crate::controller::Scene;
use crate::geom::OutputInfo;

#[cfg(feature = "layer-shell")]
pub mod layer_shell;
#[cfg(feature = "x11")]
pub mod x11;

/// What a backend can do; reported in `ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// The `backend` field of `ready`.
    pub name: &'static str,
    /// Opacity changes without redrawing (fades, pulse, capture hide).
    pub alpha_modifier: bool,
    /// The surface scale follows the compositor's fractional scale.
    pub fractional_scale: bool,
    /// The backend draws the badge (`ready` reports it).
    pub badge: bool,
    /// How often the pulsing glow is re-presented; `None`: the glow is
    /// static (its base keyframe), as without an alpha modifier.
    pub pulse_interval: Option<Duration>,
}

/// Something the backend observed while dispatching.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    /// The surfaces were configured for the first time and can be drawn.
    Configured,
    /// A frame callback fired: time for the next animation frame.
    Frame,
    /// A [`Backend::sync`] token came back: everything sent before it was
    /// processed by the display server.
    Synced(u64),
    /// The display server took the surfaces away (output unplugged, ...).
    /// The backend has dropped them already.
    Closed(String),
    /// Size or scale changed: present again.
    Changed,
    /// An output appeared, disappeared or changed (see [`Backend::outputs`]).
    OutputsChanged,
}

/// A windowing-system backend driven by [`crate::run_with`]. All methods run
/// on the loop's thread and must not block for long.
pub trait Backend {
    fn caps(&self) -> Caps;

    /// The outputs as currently known.
    fn outputs(&self) -> Vec<OutputInfo>;

    /// Flush pending requests and return the descriptor to poll for events.
    fn prepare(&mut self) -> Result<RawFd, String>;

    /// After the poll: read events if `readable`, dispatch them, and return
    /// what happened. `Err` means the connection is gone.
    fn dispatch(&mut self, readable: bool) -> Result<Vec<BackendEvent>, String>;

    /// Create the overlay on `output` (destroying existing surfaces first).
    /// Drawing starts after [`BackendEvent::Configured`].
    fn create(&mut self, output: &str) -> Result<(), String>;

    /// Destroy the overlay's surfaces, if any.
    fn destroy(&mut self);

    /// Surfaces exist and are configured.
    fn ready_to_draw(&self) -> bool;

    /// The output the surfaces are on and their current scale.
    fn current(&self) -> Option<(String, f64)>;

    /// Draw `scene`. Only what changed since the last call is redrawn and
    /// committed; a scene that is `animating` gets a frame callback, which
    /// comes back as [`BackendEvent::Frame`].
    fn present(&mut self, scene: &Scene);

    /// Request a round trip; [`BackendEvent::Synced`] with `token` comes
    /// back once the server processed everything sent before.
    fn sync(&mut self, token: u64);

    /// A backend timer (frame pacing, a held-back sync): the loop wakes up
    /// by then and calls [`Backend::dispatch`].
    fn next_wakeup(&self) -> Option<Instant> {
        None
    }
}
