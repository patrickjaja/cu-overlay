//! Standalone driver for the overlay: the same JSON-lines protocol a bridge's
//! `overlay` subcommand speaks, on this process's stdin and stdout.
//!
//! ```sh
//! printf '%s\n' '{"cmd":"show","output":"HEADLESS-1","look":"rim","arrow":{"x":200,"y":150}}' \
//!   | cu-overlay-demo
//! ```

fn main() -> std::process::ExitCode {
    cu_overlay::run(std::io::stdin(), std::io::stdout())
}
