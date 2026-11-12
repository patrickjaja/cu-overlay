//! Click-through Computer Use overlay for the claude-desktop-extra bridges:
//! upstream's glow and drawn cursor, drawn by a helper process the bridge
//! starts as its `overlay` subcommand.
//!
//! - [`render`]: the renderer core, no I/O (glow bands, rim, arrow, ring).
//! - [`anim`]: upstream's motion as pure functions of time.
//! - [`badge`]: the "Claude is using your computer" pill (layout, timing,
//!   painting with one embedded font subset).
//! - [`protocol`]: the JSON-lines wire protocol (stdin commands, stdout
//!   events).
//! - [`controller`]: the protocol state machine, backend-independent.
//! - [`backend`]: the [`backend::Backend`] seam, the Wayland layer-shell
//!   backend (`layer-shell` feature) and the XWayland backend for
//!   compositors without layer-shell (`x11` feature).
//! - [`run()`]: the run loop a bridge's `overlay` subcommand calls.
//!
//! See README.md for the wire contract.

pub mod anim;
pub mod backend;
pub mod badge;
pub mod controller;
pub mod geom;
pub mod protocol;
pub mod render;
pub(crate) mod run;
mod sys;

#[cfg(any(feature = "layer-shell", feature = "x11"))]
pub use run::run;
pub use run::{BACKEND_ENV, run_with};
pub use sys::PARENT_PID_ENV;
