//! Standalone driver for the overlay: the same JSON-lines protocol a bridge's
//! `overlay` subcommand speaks, on this process's stdin and stdout.
//!
//! ```sh
//! printf '%s\n' '{"cmd":"show","output":"HEADLESS-1","look":"rim","arrow":{"x":200,"y":150}}' \
//!   | cu-overlay-demo
//! ```

#[cfg(any(feature = "layer-shell", feature = "x11"))]
fn main() -> std::process::ExitCode {
    cu_overlay::run(std::io::stdin(), std::io::stdout())
}

#[cfg(not(any(feature = "layer-shell", feature = "x11")))]
fn main() -> std::process::ExitCode {
    println!(
        r#"{{"event":"error","message":"built without a backend (features layer-shell, x11)","fatal":true}}"#
    );
    std::process::ExitCode::from(2)
}
