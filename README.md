# cu-overlay

Shared Rust library behind the `overlay` subcommand of the [claude-desktop-extra](https://github.com/patrickjaja/claude-desktop-extra) Computer Use bridges. It draws Claude Desktop's Computer Use glow and drawn cursor on Wayland compositors, where an Electron window cannot be a click-through, always-on-top overlay: through wlr-layer-shell where the compositor has it (wlroots, KDE, Hyprland, niri, river, COSMIC), and through XWayland override-redirect windows on GNOME/Mutter, which has no layer-shell.

What it draws is upstream's own look and motion (Claude Desktop 5.1231 cu-glow page):

- **glow**: the classic pulsing glow, three inset box-shadow bands in clay `rgb(217, 119, 87)` (blur 50/100/150 px at alpha 0.5/0.3/0.1, peak 65/115/165 px at 0.7/0.5/0.2), pulsing every 2 s.
- **rim**: the own-cursor look, a 2 px solid clay rim on all four edges, plus the drawn arrow: upstream's SVG arrow (white 4.4 outline, clay body, two drop shadows), body scale 2, tilt and lift while gliding, a 13 % bump, an expanding ring (1100 ms) and a bloom (560 ms) on a press.
- **badge**: the "Claude is using your computer" pill (warm near-white `rgba(255, 247, 242, 0.95)`, 1 px clay border at 0.2, clay text and a pulsing dot). It flashes in at the centre of the output at scale 1.4, holds, and slides to the top-right corner (scale 0.8) while fading out; 6 s in all (`cubic-bezier(0.4, 0, 0.2, 1)`, keyframes 5 % / 88 % / 100 %). Upstream never shows it under reduced motion, nor does this crate.

The overlay is fully click-through: empty input region (layer-shell) or empty input shape (X11), no keyboard focus, so every click and key reaches the windows below. Nothing animates while idle: the static rim costs 0 % CPU.

Pure Rust, no C dependencies (wayland-client's pure-Rust backend, no libwayland; x11rb's pure-Rust connection, no libxcb), builds static for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`.

## Backends

| Backend | Feature | Where | `ready.backend` |
|---|---|---|---|
| layer-shell | `layer-shell` (default) | Wayland compositors with `zwlr_layer_shell_v1` | `layer-shell` |
| x11 | `x11` (opt-in) | XWayland on Wayland compositors without layer-shell: GNOME/Mutter | `x11` |

`run()` picks the backend: layer-shell when the Wayland compositor advertises `zwlr_layer_shell_v1`; else x11 when `$DISPLAY` connects, the X server is XWayland (it has the `XWAYLAND` extension) and a compositing manager owns `_NET_WM_CM_S<n>` (ARGB windows need one; Mutter does); else a fatal `error` naming every reason and exit 2. A real X11 session is never picked: there upstream's own Electron overlay runs. `CU_OVERLAY_BACKEND=layer-shell|x11` forces one backend (an explicit `x11` skips the XWayland and compositor checks, for tests on Xvfb); `auto` or unset is the rule above.

## Layout

| Module | What |
|---|---|
| `render` | Renderer core, no I/O: glow bands, rim, arrow, ring and bloom into a premultiplied BGRA buffer (`WL_SHM_FORMAT_ARGB8888` memory layout) |
| `anim` | Upstream's motion as pure functions of time: CSS cubic-bezier easings, ring/bloom/bump keyframes, glide playback, fade, pulse |
| `badge` | The badge: text layout, the flash and dot timing as pure functions of time, and the painter (pill, border, dot, text from one embedded font subset) |
| `protocol` | The JSON-lines wire protocol |
| `controller` | The protocol state machine (what is on screen at a given instant), backend-independent |
| `backend` | The `Backend` trait; `backend::layer_shell` with the `layer-shell` feature (default), `backend::x11` with the `x11` feature (`layout`: the X-to-logical mapping as pure functions) |
| `run` / `run_with` | The run loop a bridge's `overlay` subcommand calls |
| `src/bin/cu-overlay-demo.rs` | Standalone binary speaking the same protocol on its stdin/stdout |

## Dependencies

| Crate | Purpose |
|---|---|
| `wayland-client` | Wayland connection, pure-Rust backend (feature `layer-shell`) |
| `wayland-protocols` | viewporter, fractional-scale, alpha-modifier, xdg-output bindings (feature `layer-shell`) |
| `wayland-protocols-wlr` | wlr-layer-shell bindings (feature `layer-shell`) |
| `x11rb` | X11 connection, pure-Rust `RustConnection`, extensions RandR, SHAPE, MIT-SHM (feature `x11`) |
| `tiny-skia` | Anti-aliased path rasterizing for the arrow, ring, bloom and the badge's pill (no default features: no PNG) |
| `ab_glyph` | Pure-Rust glyph outlines and coverage for the badge text (no default features beyond `std`: no variable fonts) |
| `serde`, `serde_json` | The JSON-lines protocol |
| `libc` | memfd/mmap for wl_shm buffers and MIT-SHM segments, poll, `PR_SET_PDEATHSIG` |

## Protocol (version 1)

Claude Desktop spawns `<bridge> overlay` and talks to it over pipes, one JSON object per line. All coordinates are **global logical compositor coordinates** (the layout `screens` reports); the overlay converts them to output-local itself.

### stdout

| Line | When |
|---|---|
| `{"event":"ready","protocol":1,"backend":"layer-shell"\|"x11","alphaModifier":<bool>,"fractionalScale":<bool>,"badge":true,"captureScopes":["all","cursor"],"outputs":[{"name":..,"x":..,"y":..,"width":..,"height":..,"scale":..}]}` | once, after connecting. `captureScopes` lists the `capture` scopes this build accepts (feature detection). `badge` is `true` when this build draws the badge (a build or backend that does not reports `false` and ignores `badge` and `badgeText`). `width`/`height` are logical; `scale` is the output's (fractional) scale on layer-shell, and on x11 the integer factor from logical to X pixels, which is also the scale the overlay draws at (1 without Mutter's `xwayland-native-scaling`, else the ceiling of the largest monitor scale; the real per-monitor scale is not visible from X). `alphaModifier` is `true` on x11 (`_NET_WM_WINDOW_OPACITY`), `fractionalScale` `false` |
| `{"event":"outputs","outputs":[...]}` | the output layout changed after `ready` (hotplug, unplug, mode, scale, position): the full list, same shape as in `ready`. Sent only when the list actually differs from the last one reported |
| `{"event":"shown","output":"<name>","scale":<f64>}` | the overlay is on screen (after `show` or `move`); `scale` is the buffer scale in use. On x11 it follows the window mapping round trip; Mutter shows a newly mapped window up to about 140 ms later |
| `{"event":"hidden"}` | after `hide` (once the surfaces are destroyed), or after the compositor took them away |
| `{"event":"ack","id":<n>}` | a `capture` took effect and the compositor processed it |
| `{"event":"error","message":"..","fatal":<bool>}` | `fatal:false`: a malformed or unknown line, a capture auto-restore, a lost surface; the overlay keeps running. `fatal:true`: the process exits right after |

### stdin

| Command | Effect |
|---|---|
| `{"cmd":"show","output":"<name>","look":"glow"\|"rim","badge":<bool>,"badgeText":"<text>","reducedMotion":<bool>,"arrow":{"x":..,"y":..}\|null}` | Show on an output (fade in 300 ms). `look` defaults to `glow`. A `glow` show never draws the arrow; `rim` draws it at `arrow` when given, otherwise from the first `jump`/`glide`/`press`. `badge: true` flashes the badge (see Badge below); like upstream's page, every `show` restarts the flash, and a `show` with `badge` false or absent clears a running one, so send `badge: true` once per Computer Use run, not on every update. `badgeText` (optional, only with `badge: true`) replaces the built-in English text `Claude is using your computer`; control characters become spaces, it is cut at 120 characters, and empty or blank means the default. `reducedMotion`: static glow, glides jump to their last frame, a press only fades the ring (600 ms, no bloom, no bump). A `show` while shown updates look, arrow and motion setting in place; on another output it moves |
| `{"cmd":"move","output":"<name>"}` | Follow a display switch: recreate the surfaces on that output, keeping look and arrow. An unknown output is a NON-fatal `error` (`move: unknown output: <name>`); the overlay stays where it is |
| `{"cmd":"glide","frames":[[x,y,deg,lift],...],"frameMs":16}` | Play the frames `frameMs` apart (default 16, 1..1000), linearly interpolated; the last frame is the resting pose. `deg` and `lift` may be omitted (0). Up to 4096 frames. The caller computes the frames (upstream's glide planner) |
| `{"cmd":"jump","x":..,"y":..}` | Place the arrow, no animation |
| `{"cmd":"press","x":..,"y":..}` | Snap the arrow there and play ring + bloom + bump. The ring stays at the press point if a glide follows |
| `{"cmd":"look","look":"glow"\|"rim"}` | Switch look |
| `{"cmd":"hide"}` | Fade out (300 ms), destroy the surfaces, then `hidden`. A later `show` creates new surfaces |
| `{"cmd":"capture","phase":"hide"\|"restore","id":<n>,"scope":"all"\|"cursor"}` | Around a screenshot. `hide` with `scope` `all` (the default when `scope` is omitted) makes everything fully transparent; `cursor` removes only the drawn cursor (arrow and press ring) and leaves the rim or glow up, so the indicator does not blink at every capture. `restore` ends any hide and puts back what it removed at its CURRENT state: the arrow at its present pose (a glide keeps its clock while hidden), the ring only if its animation is still running (a finished ring is not replayed). Each is acked with its `id` once the compositor processed it (layer-shell: the commit, then a `wl_display.sync` round trip; measured on headless sway over 50 rounds: hide->ack median 0.4 ms, max 1.4 ms, both scopes. x11: the opacity change, an X round trip and, for a hide, 50 ms for Mutter to repaint; measured on GNOME 48 against ScreenCast frames: the clean frame arrived 11-37 ms after the hide, the ack 50-64 ms after it, never a frame with the hidden part after the ack, both scopes, with and without native scaling). Not nested: a second `hide` before `restore` acks again (the larger scope wins, the timer restarts) and one `restore` ends it. A `hide` with nothing of its scope visible acks just the same. A `hide` without `restore` restores itself after 3 s and reports a non-fatal error, for both scopes. Works while hidden too (a `show` during an `all` hide stays invisible until restore) |

**Badge.** The flash starts at a `show` with `badge: true` and runs for 6 s, then the surface is blanked and nothing animates (idle CPU is 0 again). It is placed from the shown output's logical size: centred at scale 1.4 until 5.3 s, then sliding to `top: 40px; left: 100% - 280px` at scale 0.8 while fading out (a long text keeps 8 px to the output's right edge; a pill that does not fit the output is not drawn). `reducedMotion: true` never shows it. It follows the show/hide fade and survives a `move` (the clock keeps running, the pill is re-placed on the new output). Capture: scope `all` hides it with everything else, scope `cursor` leaves it, because the badge is not a cursor and is short-lived; callers that must keep it out of every screenshot use `all`. Its surface is click-through like the others, confined to the shown output (a box the size of the pill at its largest scale, clamped inside the output) and drawn at the output's exact fractional scale. It stacks below the press ring and the arrow.

**Arrow points off the shown output.** Everything the overlay draws stays on the output it is shown on. A `jump`, `press`, glide frame or `show` arrow whose point lies on another output (or nowhere) is accepted and its pose kept, but neither the arrow nor the ring is drawn for it. While the drawn cursor is on (shown, `rim` look), each such command produces at most one `{"event":"error","message":"arrow outside output <name>","fatal":false}`, not one per frame. A glide that leaves the output hides the arrow for the frames off it. After a `move` to the output the kept pose is on, the arrow is drawn there without being resent. A point on the output close to its edge is drawn cut at the edge: the arrow's and ring's surfaces are clipped to the output, so nothing spills onto a neighbouring output.

Unknown commands and malformed lines (bad JSON, missing fields, out-of-range numbers) produce a non-fatal `error` and are otherwise ignored; unknown extra fields are ignored. Lines over 256 KiB are dropped with an error.

### Lifetime

| Cause | Exit code |
|---|---|
| stdin EOF (a last line without newline still runs) | 0 |
| parent death: `PR_SET_PDEATHSIG` = SIGTERM, plus a check every 500 ms of `CLAUDE_CU_PARENT_PID` (by pid and start time, as in cu-abort-watch) or of `getppid` | 0 (SIGTERM: killed by the signal) |
| no backend fits (no Wayland display or no wlr-layer-shell, and no XWayland with a compositing manager) | 2, after a fatal `error` naming every reason; nothing created |
| `show` naming an output that does not exist | 2, after `{"event":"error","message":"unknown output: <name>","fatal":true}` |
| Wayland or X connection lost, stdout closed | 1 |

The surfaces (windows) belong to the process's display connection, so any exit (also SIGKILL) removes them. An output that is unplugged while shown closes the surfaces: a non-fatal `error` (`the compositor closed the overlay surface`, on x11 `the output of the overlay was removed`), `hidden` and an `outputs` event without it follow (x11 sends the `outputs` event first); the process stays alive and the bridge sends `show` again for another output.

To map a display to an output name, use `ready`'s `outputs` and every later `outputs` event: each entry is the output's name with its global logical `x`, `y`, `width`, `height` and `scale`, the same layout `screens` reports.

## Use from a bridge

```toml
[dependencies]
# local development
cu-overlay = { path = "../cu-overlay" }
# at ship time
# cu-overlay = { git = "https://github.com/patrickjaja/cu-overlay", rev = "<pinned rev>" }
# the GNOME bridge: the XWayland backend only (Mutter has no layer-shell)
# cu-overlay = { path = "../cu-overlay", default-features = false, features = ["x11"] }
```

```rust
// main.rs: the `overlay` subcommand
Command::Overlay => return cu_overlay::run(std::io::stdin(), std::io::stdout()),
```

```rust
pub fn run<R: AsFd, W: Write>(input: R, output: W) -> std::process::ExitCode;  // picks the backend
pub fn run_with<B: Backend, R: AsFd, W: Write>(backend: B, input: R, output: W) -> ExitCode;
pub const PARENT_PID_ENV: &str = "CLAUDE_CU_PARENT_PID";
pub const BACKEND_ENV: &str = "CU_OVERLAY_BACKEND";
// feature x11
pub fn backend::x11::X11::connect(opts: backend::x11::Options) -> Result<X11, String>;
```

- `run` blocks, needs no async runtime and opens its own Wayland connection. In a tokio bridge (kwin-portal-bridge) call it before building the runtime or from `spawn_blocking`.
- Nothing else may write to stdout while it runs; log to stderr. Return its exit code from `main` right away.
- Claude Desktop should set `CLAUDE_CU_PARENT_PID` to its own pid in the spawn environment.
- The overlay's surfaces use the layer-shell namespace `claude-cu-overlay`. A bridge whose screenshots go through the compositor can also leave them out itself; `capture` covers every capture path.
- On x11 every overlay window has `WM_CLASS` and `_NET_WM_NAME` `claude-cu-overlay`, `_NET_WM_PID` of the overlay process, `_NET_WM_WINDOW_TYPE_NOTIFICATION` and an empty input shape. On GNOME the windows are in every capture (portal ScreenCast and Shell screenshots) and nothing can exclude them: the caller must hide them around each screenshot with `capture` (scope `all` when the glow or the badge must not be in it, `cursor` to keep the rim and glow in the screenshot).
- **GNOME bridge (x11).** Mutter's physical layout mode (`scale-monitor-framebuffer` off, scale > 1) looks exactly like native scaling from X but its layout is physical pixels. The bridge knows the layout mode from `org.gnome.Mutter.DisplayConfig` and passes the factor: `X11::connect(Options { scale: Some(1), .. })` there (or `CU_OVERLAY_X11_SCALE=1`). In the logical layout mode the detection from X is right and nothing needs to be passed. `ready.outputs` names are the connector names, as `screens` reports them.

## Build and test

```bash
cargo build --release --target x86_64-unknown-linux-musl     # static(-pie) demo binary
cargo build --release --target aarch64-unknown-linux-musl    # same rust-lld config as the bridges
cargo build --release --target x86_64-unknown-linux-musl --features x11   # demo with both backends
cargo test --features x11                                     # unit tests (no compositor or X server needed)
cargo test --no-default-features                              # renderer core + protocol only
cargo fmt --all -- --check && cargo clippy --all-targets --features x11 -- -D warnings

# x11 backend end-to-end against a throwaway Xvfb (forced backend, single monitor):
scripts/test-xvfb.sh                     # needs xvfb, x11-utils, x11-xserver-utils, xdotool, python3-pil;
                                         # picom + imagemagick add pixel checks

# end-to-end against a headless nested sway (no window on the host):
scripts/test-headless-sway.sh            # needs sway, python3-pil, wlroots-bridge, foot (click test)
scripts/test-headless-sway.sh --cpu      # also measures the overlay's CPU over 10 s per scenario
EVIDENCE_DIR=/tmp/ev scripts/test-headless-sway.sh   # keep the screenshots

cargo run --release --example snapshot -- /tmp/snap 2   # raw BGRA renders of arrow, ring, glow
cargo run --release --example bench -- 1.5              # per-frame draw cost
```

The script prints the sway and wlroots versions, the empty-output colour and `alphaModifier` first. "Nothing of ours here" assertions compare against a screenshot of each output taken before any overlay exists (black on sway 1.10 and later, dark gray (63,63,63) on the sway 1.9 of ubuntu-24.04), and the pulse check follows `alphaModifier` (no alpha modifier: the glow must be static). `CU_OVERLAY_DISABLE=alpha-modifier scripts/test-headless-sway.sh` runs the whole suite without alpha-modifier, as on wlroots 0.17. CI uploads the screenshots as the `headless-sway-evidence` artifact on every run.

Debugging: `CU_OVERLAY_DEBUG=1` prints redraw and commit counts to stderr at exit (x11: puts per window and opacity changes, and logs X errors); `CU_OVERLAY_DISABLE=alpha-modifier,fractional-scale,viewporter` ignores those globals to exercise the fallbacks. x11 only: `CU_OVERLAY_X11_SCALE=<n>` forces the X-to-logical factor, `CU_OVERLAY_X11_PULSE_MS=<ms>` turns the glow pulse on.

The Xvfb test checks the selection guard (auto mode refuses a plain X server), `ready`, the four windows (override-redirect, depth 32, names, pid, window type, `_NET_WM_BYPASS_COMPOSITOR`, geometry from the logical layout), that the pointer over the arrow and the rim is over the root (empty input shape), the opacity of every window through `capture` hide and restore in both scopes, the factor from `Xft.dpi`, a stale RandR corrected through `_NET_DESKTOP_GEOMETRY` and fitted to `_GTK_WORKAREAS_D0` (set by hand as Mutter would), an unplug and replug of the shown output, and nothing left after `hide` and stdin EOF; with picom (xrender) the pixels of the rim, arrow, transparency, both capture scopes, the badge and the translucent glow. Xvfb cannot give multi-monitor RandR monitors; that, Mutter's real behaviour and the capture timing are verified on GNOME (DESIGN.md).

The integration test drives `cu-overlay-demo` on two headless outputs (the second at x = 1280 and scale 1.5) and asserts on `wlroots-bridge screenshot` captures: rim on all four edges, glow bands and pulse, the arrow at the requested global point on both outputs, the press ring, a glide's mid-flight and resting pose, full transparency during a capture hide, a `scope:"cursor"` capture hide that keeps the rim on all four edges with no arrow or ring pixels and restores the arrow at its pose, the 3 s auto-restore, the hide->ack latency (50 rounds per scope), a `move` to an unknown output staying non-fatal, the arrow confined to its output (jump + press on the neighbouring output draw nothing there and report one non-fatal error each; after `move` the kept arrow appears on the new output; a press straddling the shared edge from either side leaves no pixel on the neighbour; a glide leaving the output hides the arrow), the badge (flash-in at the centre on both outputs, the pill confined to its output, `scope:"cursor"` leaving it and `scope:"all"` removing it, the slide at 5.6 s, gone after 6 s, no badge under `reducedMotion`, `badgeText`, a `show` without badge clearing it, also without alpha-modifier), a hot-plugged third output reported in an `outputs` event and then unplugged while shown (non-fatal error, `hidden`, process alive, `show` on a remaining output works), nothing left after `hide` or parent death, a click at the arrow reaching the window below (sway seat + two `foot` windows, focus checked via `swaymsg -t get_tree`), and the fallbacks without alpha-modifier and without viewporter.

## Fallbacks and limits

- **GNOME/Mutter (x11 backend).** The overlay windows are hidden in the activities overview, below GNOME Shell's modal dialogs and notification banners, and covered by the lock screen. They are in every capture (portal ScreenCast and Shell screenshots) unless hidden with `capture`. Without the experimental `xwayland-native-scaling` Mutter upscales X windows, so on a fractionally scaled monitor the glow and the cursor are drawn at scale 1 and look blurry; with it they are drawn at the integer factor and downscaled. The glow is static by default (each pulse step makes Mutter repaint the whole output; see DESIGN.md); `CU_OVERLAY_X11_PULSE_MS` turns the pulse on. XWayland starts on demand: a cold connect takes 180-270 ms. A build without the `x11` feature fails on GNOME with a fatal `error` and exit 2.
- **No `wp_alpha_modifier_v1`**: show and hide are instant (no fade), the glow is static at upstream's base keyframe (its reduced-motion look), and a capture hide swaps in a transparent buffer instead of a zero multiplier. The arrow and ring still animate.
- **No `wp_viewporter` / `wp_fractional_scale_v1`**: buffers are drawn at the output's integer scale (`wl_surface.set_buffer_scale`), so a 1.5 output gets scale-2 buffers downscaled by the compositor.
- **Badge text coverage**: the embedded font is a subset of Noto Sans Medium (Basic Latin, Latin-1, Latin Extended-A, Greek, Cyrillic, general punctuation, the euro sign). Other scripts (CJK, Arabic, Hebrew, Indic, ...) draw as the font's `.notdef` box; upstream's own string is English only. There is no kerning or shaping. Upstream's page uses the system sans-serif at weight 600; this crate always uses its embedded face at weight 500, so the text is a little narrower and lighter than on a given desktop.
- The glow pulse is approximated with an alpha multiplier on the peak keyframe (0.70 to 1.0) instead of interpolating the blur radii.

## License

MIT, see [LICENSE](LICENSE).

The badge text is drawn with a subset of Noto Sans Medium, Copyright 2022 The Noto Project Authors, licensed under the SIL Open Font License 1.1 (full text in [assets/OFL.txt](assets/OFL.txt)). The font file is embedded unchanged apart from the subsetting (no layout tables, no hinting).
