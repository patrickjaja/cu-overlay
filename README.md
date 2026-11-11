# cu-overlay

Shared Rust library behind the `overlay` subcommand of the [claude-desktop-extra](https://github.com/patrickjaja/claude-desktop-extra) Computer Use bridges. It draws Claude Desktop's Computer Use glow and drawn cursor on Wayland compositors that have wlr-layer-shell, where an Electron window cannot be a click-through, always-on-top overlay.

What it draws is upstream's own look and motion (Claude Desktop 5.1231 cu-glow page):

- **glow**: the classic pulsing glow, three inset box-shadow bands in clay `rgb(217, 119, 87)` (blur 50/100/150 px at alpha 0.5/0.3/0.1, peak 65/115/165 px at 0.7/0.5/0.2), pulsing every 2 s.
- **rim**: the own-cursor look, a 2 px solid clay rim on all four edges, plus the drawn arrow: upstream's SVG arrow (white 4.4 outline, clay body, two drop shadows), body scale 2, tilt and lift while gliding, a 13 % bump, an expanding ring (1100 ms) and a bloom (560 ms) on a press.

The overlay surface is fully click-through: empty input region, no keyboard interactivity, exclusive zone -1, so every click and key reaches the windows below. Nothing animates while idle: the static rim costs 0 % CPU.

Pure Rust, no C dependencies (wayland-client's pure-Rust backend, no libwayland), builds static for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`.

## Layout

| Module | What |
|---|---|
| `render` | Renderer core, no I/O: glow bands, rim, arrow, ring and bloom into a premultiplied BGRA buffer (`WL_SHM_FORMAT_ARGB8888` memory layout) |
| `anim` | Upstream's motion as pure functions of time: CSS cubic-bezier easings, ring/bloom/bump keyframes, glide playback, fade, pulse |
| `protocol` | The JSON-lines wire protocol |
| `controller` | The protocol state machine (what is on screen at a given instant), backend-independent |
| `backend` | The `Backend` trait; `backend::layer_shell` with the `layer-shell` feature (default) |
| `run` / `run_with` | The run loop a bridge's `overlay` subcommand calls |
| `src/bin/cu-overlay-demo.rs` | Standalone binary speaking the same protocol on its stdin/stdout |

## Dependencies

| Crate | Purpose |
|---|---|
| `wayland-client` | Wayland connection, pure-Rust backend (feature `layer-shell`) |
| `wayland-protocols` | viewporter, fractional-scale, alpha-modifier, xdg-output bindings (feature `layer-shell`) |
| `wayland-protocols-wlr` | wlr-layer-shell bindings (feature `layer-shell`) |
| `tiny-skia` | Anti-aliased path rasterizing for the arrow, ring and bloom (no default features: no PNG) |
| `serde`, `serde_json` | The JSON-lines protocol |
| `libc` | memfd/mmap for wl_shm buffers, poll, `PR_SET_PDEATHSIG` |

## Protocol (version 1)

Claude Desktop spawns `<bridge> overlay` and talks to it over pipes, one JSON object per line. All coordinates are **global logical compositor coordinates** (the layout `screens` reports); the overlay converts them to output-local itself.

### stdout

| Line | When |
|---|---|
| `{"event":"ready","protocol":1,"backend":"layer-shell","alphaModifier":<bool>,"fractionalScale":<bool>,"badge":false,"outputs":[{"name":..,"x":..,"y":..,"width":..,"height":..,"scale":..}]}` | once, after connecting. `width`/`height` are logical; `scale` is the output's (fractional) scale |
| `{"event":"outputs","outputs":[...]}` | the output layout changed after `ready` (hotplug, unplug, mode, scale, position): the full list, same shape as in `ready`. Sent only when the list actually differs from the last one reported |
| `{"event":"shown","output":"<name>","scale":<f64>}` | the overlay is on screen (after `show` or `move`); `scale` is the buffer scale in use |
| `{"event":"hidden"}` | after `hide` (once the surfaces are destroyed), or after the compositor took them away |
| `{"event":"ack","id":<n>}` | a `capture` took effect and the compositor processed it |
| `{"event":"error","message":"..","fatal":<bool>}` | `fatal:false`: a malformed or unknown line, a capture auto-restore, a lost surface; the overlay keeps running. `fatal:true`: the process exits right after |

### stdin

| Command | Effect |
|---|---|
| `{"cmd":"show","output":"<name>","look":"glow"\|"rim","badge":<bool>,"reducedMotion":<bool>,"arrow":{"x":..,"y":..}\|null}` | Show on an output (fade in 300 ms). `look` defaults to `glow`. A `glow` show never draws the arrow; `rim` draws it at `arrow` when given, otherwise from the first `jump`/`glide`/`press`. `badge` is accepted and ignored in protocol 1 (see Limits). `reducedMotion`: static glow, glides jump to their last frame, a press only fades the ring (600 ms, no bloom, no bump). A `show` while shown updates look, arrow and motion setting in place; on another output it moves |
| `{"cmd":"move","output":"<name>"}` | Follow a display switch: recreate the surfaces on that output, keeping look and arrow. An unknown output is a NON-fatal `error` (`move: unknown output: <name>`); the overlay stays where it is |
| `{"cmd":"glide","frames":[[x,y,deg,lift],...],"frameMs":16}` | Play the frames `frameMs` apart (default 16, 1..1000), linearly interpolated; the last frame is the resting pose. `deg` and `lift` may be omitted (0). Up to 4096 frames. The caller computes the frames (upstream's glide planner) |
| `{"cmd":"jump","x":..,"y":..}` | Place the arrow, no animation |
| `{"cmd":"press","x":..,"y":..}` | Snap the arrow there and play ring + bloom + bump. The ring stays at the press point if a glide follows |
| `{"cmd":"look","look":"glow"\|"rim"}` | Switch look |
| `{"cmd":"hide"}` | Fade out (300 ms), destroy the surfaces, then `hidden`. A later `show` creates new surfaces |
| `{"cmd":"capture","phase":"hide"\|"restore","id":<n>}` | Around a screenshot: `hide` makes everything fully transparent, `restore` puts it back; each is acked with its `id` once the compositor processed it (a `wl_display.sync` round trip after the commit). A `hide` without `restore` restores itself after 3 s and reports a non-fatal error. Works while hidden too (a `show` during a capture hide stays invisible until restore) |

**Arrow points off the shown output.** Everything the overlay draws stays on the output it is shown on. A `jump`, `press`, glide frame or `show` arrow whose point lies on another output (or nowhere) is accepted and its pose kept, but neither the arrow nor the ring is drawn for it. While the drawn cursor is on (shown, `rim` look), each such command produces at most one `{"event":"error","message":"arrow outside output <name>","fatal":false}`, not one per frame. A glide that leaves the output hides the arrow for the frames off it. After a `move` to the output the kept pose is on, the arrow is drawn there without being resent. A point on the output close to its edge is drawn cut at the edge: the arrow's and ring's surfaces are clipped to the output, so nothing spills onto a neighbouring output.

Unknown commands and malformed lines (bad JSON, missing fields, out-of-range numbers) produce a non-fatal `error` and are otherwise ignored; unknown extra fields are ignored. Lines over 256 KiB are dropped with an error.

### Lifetime

| Cause | Exit code |
|---|---|
| stdin EOF (a last line without newline still runs) | 0 |
| parent death: `PR_SET_PDEATHSIG` = SIGTERM, plus a check every 500 ms of `CLAUDE_CU_PARENT_PID` (by pid and start time, as in cu-abort-watch) or of `getppid` | 0 (SIGTERM: killed by the signal) |
| no Wayland display, no wlr-layer-shell | 2, after a fatal `error`; nothing created |
| `show` naming an output that does not exist | 2, after `{"event":"error","message":"unknown output: <name>","fatal":true}` |
| Wayland connection lost, stdout closed | 1 |

The surfaces belong to the process's Wayland connection, so any exit (also SIGKILL) removes them. An output that is unplugged while shown closes the surfaces: a non-fatal `error` (`the compositor closed the overlay surface`), `hidden` and an `outputs` event without it follow; the process stays alive and the bridge sends `show` again for another output.

To map a display to an output name, use `ready`'s `outputs` and every later `outputs` event: each entry is the output's name with its global logical `x`, `y`, `width`, `height` and `scale`, the same layout `screens` reports.

## Use from a bridge

```toml
[dependencies]
# local development
cu-overlay = { path = "../cu-overlay" }
# at ship time
# cu-overlay = { git = "https://github.com/patrickjaja/cu-overlay", rev = "<pinned rev>" }
```

```rust
// main.rs: the `overlay` subcommand
Command::Overlay => return cu_overlay::run(std::io::stdin(), std::io::stdout()),
```

```rust
pub fn run<R: AsFd, W: Write>(input: R, output: W) -> std::process::ExitCode;  // layer-shell backend
pub fn run_with<B: Backend, R: AsFd, W: Write>(backend: B, input: R, output: W) -> ExitCode;
pub const PARENT_PID_ENV: &str = "CLAUDE_CU_PARENT_PID";
```

- `run` blocks, needs no async runtime and opens its own Wayland connection. In a tokio bridge (kwin-portal-bridge) call it before building the runtime or from `spawn_blocking`.
- Nothing else may write to stdout while it runs; log to stderr. Return its exit code from `main` right away.
- Claude Desktop should set `CLAUDE_CU_PARENT_PID` to its own pid in the spawn environment.
- The overlay's surfaces use the layer-shell namespace `claude-cu-overlay`. A bridge whose screenshots go through the compositor can also leave them out itself; `capture` covers every capture path.

## Build and test

```bash
cargo build --release --target x86_64-unknown-linux-musl     # static(-pie) demo binary
cargo build --release --target aarch64-unknown-linux-musl    # same rust-lld config as the bridges
cargo test                                                    # unit tests (no compositor needed)
cargo test --no-default-features                              # renderer core + protocol only
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings

# end-to-end against a headless nested sway (no window on the host):
scripts/test-headless-sway.sh            # needs sway, python3-pil, wlroots-bridge, foot (click test)
scripts/test-headless-sway.sh --cpu      # also measures the overlay's CPU over 10 s per scenario
EVIDENCE_DIR=/tmp/ev scripts/test-headless-sway.sh   # keep the screenshots

cargo run --release --example snapshot -- /tmp/snap 2   # raw BGRA renders of arrow, ring, glow
cargo run --release --example bench -- 1.5              # per-frame draw cost
```

Debugging: `CU_OVERLAY_DEBUG=1` prints redraw and commit counts to stderr at exit; `CU_OVERLAY_DISABLE=alpha-modifier,fractional-scale,viewporter` ignores those globals to exercise the fallbacks.

The integration test drives `cu-overlay-demo` on two headless outputs (the second at x = 1280 and scale 1.5) and asserts on `wlroots-bridge screenshot` captures: rim on all four edges, glow bands and pulse, the arrow at the requested global point on both outputs, the press ring, a glide's mid-flight and resting pose, full transparency during a capture hide, the 3 s auto-restore, a `move` to an unknown output staying non-fatal, the arrow confined to its output (jump + press on the neighbouring output draw nothing there and report one non-fatal error each; after `move` the kept arrow appears on the new output; a press straddling the shared edge from either side leaves no pixel on the neighbour; a glide leaving the output hides the arrow), a hot-plugged third output reported in an `outputs` event and then unplugged while shown (non-fatal error, `hidden`, process alive, `show` on a remaining output works), nothing left after `hide` or parent death, a click at the arrow reaching the window below (sway seat + two `foot` windows, focus checked via `swaymsg -t get_tree`), and the fallbacks without alpha-modifier and without viewporter.

## Fallbacks and limits

- **Compositors without wlr-layer-shell** (GNOME/Mutter): the backend fails with a fatal `error` and exit 2. An X11 override-redirect backend for GNOME is planned behind its own cargo feature (see DESIGN.md).
- **No `wp_alpha_modifier_v1`**: show and hide are instant (no fade), the glow is static at upstream's base keyframe (its reduced-motion look), and a capture hide swaps in a transparent buffer instead of a zero multiplier. The arrow and ring still animate.
- **No `wp_viewporter` / `wp_fractional_scale_v1`**: buffers are drawn at the output's integer scale (`wl_surface.set_buffer_scale`), so a 1.5 output gets scale-2 buffers downscaled by the compositor.
- **No badge**: upstream's "Claude is using your computer" pill needs text rendering; protocol 1 accepts `badge` and ignores it (`ready` reports `"badge":false`).
- The glow pulse is approximated with an alpha multiplier on the peak keyframe (0.70 to 1.0) instead of interpolating the blur radii.

## License

MIT, see [LICENSE](LICENSE).
