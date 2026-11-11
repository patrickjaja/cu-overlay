//! Click-through Computer Use overlay for the claude-desktop-extra bridges:
//! upstream's glow and drawn cursor, drawn by a helper process the bridge
//! starts as its `overlay` subcommand.
//!
//! - [`render`]: the renderer core, no I/O (glow bands, rim, arrow, ring).
//! - [`anim`]: upstream's motion as pure functions of time.
//! - [`protocol`]: the JSON-lines wire protocol (stdin commands, stdout
//!   events).
//! - [`controller`]: the protocol state machine, backend-independent.
//! - [`backend`]: the [`backend::Backend`] seam and, with the `layer-shell`
//!   feature, the Wayland layer-shell backend.
//! - [`run()`]: the run loop a bridge's `overlay` subcommand calls.
//!
//! See README.md for the wire contract.

pub mod anim;
pub mod backend;
pub mod controller;
pub mod geom;
pub mod protocol;
pub mod render;
mod run;
mod sys;

#[cfg(feature = "layer-shell")]
pub use run::run;
pub use run::run_with;
pub use sys::PARENT_PID_ENV;
