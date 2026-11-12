//! The run loop: stdin commands -> [`Controller`] -> [`Backend`], backend
//! events and timers -> stdout events. Single-threaded and idle (no wakeups
//! besides a 500 ms parent check) while nothing animates.

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::backend::{Backend, BackendEvent};
use crate::controller::{Action, Controller, Scene};
use crate::geom::OutputInfo;
use crate::protocol::{Event, MAX_LINE, PROTOCOL, parse_command};
use crate::sys::{self, Parent, ReadResult};

/// Parent check interval (as in cu-abort-watch).
const PARENT_CHECK: Duration = Duration::from_millis(500);
/// Glow pulse redraw interval on layer-shell (alpha multiplier only, no
/// buffer redraw) and the badge dot's interval on every backend.
pub(crate) const PULSE_INTERVAL: Duration = Duration::from_millis(33);
/// An animation whose frame callbacks stall (output off, surface occluded)
/// still advances at this rate, so its end state lands.
const STALL_INTERVAL: Duration = Duration::from_millis(250);

/// Splits stdin bytes into lines, bounding line length.
#[derive(Default)]
pub(crate) struct LineReader {
    buf: Vec<u8>,
    discarding: bool,
}

impl LineReader {
    /// Feed bytes; returns complete lines (`Err` for an over-long one).
    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<Result<String, String>> {
        let mut out = Vec::new();
        for &b in data {
            if b == b'\n' {
                if self.discarding {
                    self.discarding = false;
                } else {
                    out.push(Ok(String::from_utf8_lossy(&self.buf).into_owned()));
                }
                self.buf.clear();
            } else if !self.discarding {
                self.buf.push(b);
                if self.buf.len() > MAX_LINE {
                    self.buf.clear();
                    self.discarding = true;
                    out.push(Err(format!("line longer than {MAX_LINE} bytes ignored")));
                }
            }
        }
        out
    }

    /// The last line at EOF (no trailing newline).
    pub(crate) fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buf);
        (!self.discarding && !rest.is_empty()).then(|| String::from_utf8_lossy(&rest).into_owned())
    }
}

enum Pending {
    Ack(u64),
    Hidden,
    Shown,
}

struct Loop<B: Backend, W: Write> {
    be: B,
    out: W,
    ctl: Controller,
    pending: HashMap<u64, Pending>,
    next_token: u64,
    dirty: bool,
    /// Configured, `shown` not yet requested.
    shown_due: bool,
    last_present: Instant,
    last_scene: Option<Scene>,
    exit: Option<u8>,
    /// The output list last reported (`ready` or `outputs`).
    sent_outputs: Vec<OutputInfo>,
    /// The backend's glow pulse interval.
    pulse_interval: Option<Duration>,
}

impl<B: Backend, W: Write> Loop<B, W> {
    fn emit(&mut self, ev: &Event) {
        let ok = self.out.write_all(ev.line().as_bytes()).is_ok() && self.out.flush().is_ok();
        if !ok && self.exit.is_none() {
            // stdout is gone: nobody can drive the overlay any more
            self.exit = Some(1);
        }
    }

    fn sync(&mut self, p: Pending) {
        let t = self.next_token;
        self.next_token += 1;
        self.pending.insert(t, p);
        self.be.sync(t);
    }

    fn present(&mut self, now: Instant) {
        self.dirty = false;
        self.last_present = now;
        let scene = self.ctl.scene(now);
        if let Some(s) = &scene
            && self.be.ready_to_draw()
        {
            self.be.present(s);
            if self.shown_due {
                self.shown_due = false;
                self.sync(Pending::Shown);
            }
        }
        self.last_scene = scene;
    }

    fn apply(&mut self, actions: Vec<Action>, now: Instant) {
        for a in actions {
            match a {
                Action::Emit(ev) => self.emit(&ev),
                Action::Create(output) => {
                    self.shown_due = false;
                    if let Err(e) = self.be.create(&output) {
                        self.emit(&Event::error(e, true));
                        self.exit.get_or_insert(2);
                    }
                }
                Action::Destroy => {
                    self.be.destroy();
                    self.shown_due = false;
                    self.sync(Pending::Hidden);
                }
                Action::Ack(id) => {
                    // the new state goes out before the round trip
                    self.present(now);
                    self.sync(Pending::Ack(id));
                }
                Action::Redraw => self.dirty = true,
                Action::Exit(code) => {
                    self.exit.get_or_insert(code as u8);
                }
            }
            if self.exit.is_some() {
                return;
            }
        }
    }

    fn command(&mut self, line: Result<String, String>, now: Instant) {
        let line = match line {
            Ok(l) => l,
            Err(e) => return self.emit(&Event::error(e, false)),
        };
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match parse_command(line) {
            Ok(cmd) => {
                let outputs = self.be.outputs();
                let actions = self.ctl.handle(cmd, now, &outputs);
                self.apply(actions, now);
            }
            Err(e) => self.emit(&Event::error(e, false)),
        }
    }

    fn backend_event(&mut self, ev: BackendEvent) {
        match ev {
            BackendEvent::Configured => {
                self.shown_due = true;
                self.dirty = true;
            }
            BackendEvent::Frame | BackendEvent::Changed => self.dirty = true,
            BackendEvent::OutputsChanged => {
                self.dirty = true;
                let now = self.be.outputs();
                if now != self.sent_outputs {
                    self.sent_outputs = now.clone();
                    self.emit(&Event::Outputs { outputs: now });
                }
            }
            BackendEvent::Synced(t) => match self.pending.remove(&t) {
                Some(Pending::Ack(id)) => self.emit(&Event::Ack { id }),
                Some(Pending::Hidden) => self.emit(&Event::Hidden),
                Some(Pending::Shown) => {
                    if let Some((output, scale)) = self.be.current() {
                        self.emit(&Event::Shown { output, scale });
                    }
                }
                None => {}
            },
            BackendEvent::Closed(msg) => {
                self.ctl.surfaces_lost();
                self.be.destroy();
                self.emit(&Event::error(msg, false));
                self.emit(&Event::Hidden);
            }
        }
    }

    /// How often a scene that is not frame-paced is re-presented: the badge
    /// dot at [`PULSE_INTERVAL`], the glow pulse at the backend's rate.
    fn redraw_interval(&self, s: &Scene) -> Option<Duration> {
        let badge = s.badge.is_some().then_some(PULSE_INTERVAL);
        let pulse = s.pulse.and(self.pulse_interval);
        match (badge, pulse) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn timeout(&self, now: Instant) -> Duration {
        let mut d = PARENT_CHECK;
        let mut at = |t: Instant| d = d.min(t.saturating_duration_since(now));
        if let Some(t) = self.ctl.next_deadline() {
            at(t);
        }
        if let Some(t) = self.be.next_wakeup() {
            at(t);
        }
        if let Some(s) = &self.last_scene
            && self.be.ready_to_draw()
        {
            if let Some(i) = self.redraw_interval(s) {
                at(self.last_present + i);
            }
            if s.animating {
                at(self.last_present + STALL_INTERVAL);
            }
        }
        if self.dirty {
            d = Duration::ZERO;
        }
        d
    }
}

/// Run the overlay on `backend` until stdin closes: commands from `input`,
/// events to `output`. Emits `ready` first. The process should exit with
/// the returned code right after (the surfaces die with the connection).
///
/// Exit codes: 0 stdin EOF or parent gone; 1 display connection or stdout
/// lost; 2 unknown output.
pub fn run_with<B: Backend, R: AsFd, W: Write>(backend: B, input: R, output: W) -> ExitCode {
    let parent = Parent::watch();
    if parent.gone() {
        return ExitCode::SUCCESS;
    }
    let in_fd = input.as_fd().as_raw_fd();
    let now = Instant::now();
    let caps = backend.caps();
    let mut l = Loop {
        ctl: Controller::new(caps.alpha_modifier, now).with_pulse(caps.pulse_interval.is_some()),
        be: backend,
        out: output,
        pending: HashMap::new(),
        next_token: 1,
        dirty: false,
        shown_due: false,
        last_present: now,
        last_scene: None,
        exit: None,
        sent_outputs: Vec::new(),
        pulse_interval: caps.pulse_interval,
    };
    let ready = Event::Ready {
        protocol: PROTOCOL,
        backend: caps.name,
        alpha_modifier: caps.alpha_modifier,
        fractional_scale: caps.fractional_scale,
        badge: caps.badge,
        capture_scopes: crate::protocol::CAPTURE_SCOPES,
        outputs: l.be.outputs(),
    };
    if let Event::Ready { outputs, .. } = &ready {
        l.sent_outputs = outputs.clone();
    }
    l.emit(&ready);

    let mut lines = LineReader::default();
    let mut stdin_open = true;
    let mut last_parent_check = now;
    while l.exit.is_none() {
        let now = Instant::now();
        let actions = l.ctl.tick(now);
        l.apply(actions, now);
        if l.exit.is_some() {
            break;
        }
        // frame-paced animation, pulse and stall timers
        if let Some(s) = &l.last_scene {
            let since = now.saturating_duration_since(l.last_present);
            if l.redraw_interval(s).is_some_and(|i| since >= i)
                || (s.animating && since >= STALL_INTERVAL)
            {
                l.dirty = true;
            }
        }
        if l.dirty {
            l.present(now);
        }
        let fd = match l.be.prepare() {
            Ok(fd) => fd,
            Err(e) => {
                l.emit(&Event::error(e, true));
                l.exit.get_or_insert(1);
                break;
            }
        };
        let ready = sys::poll2(fd, stdin_open.then_some(in_fd), Some(l.timeout(now)));
        let events = match l.be.dispatch(ready.display) {
            Ok(ev) => ev,
            Err(e) => {
                l.emit(&Event::error(e, true));
                l.exit.get_or_insert(1);
                break;
            }
        };
        for ev in events {
            l.backend_event(ev);
        }
        if ready.input {
            let now = Instant::now();
            let mut buf = [0u8; 16 * 1024];
            match sys::read_fd(in_fd, &mut buf) {
                ReadResult::Data(n) => {
                    for line in lines.push(&buf[..n]) {
                        l.command(line, now);
                        if l.exit.is_some() {
                            break;
                        }
                    }
                }
                ReadResult::Again => {}
                ReadResult::Eof => {
                    if let Some(last) = lines.finish() {
                        l.command(Ok(last), now);
                    }
                    stdin_open = false;
                    l.exit.get_or_insert(0);
                }
            }
        }
        let now = Instant::now();
        if now.saturating_duration_since(last_parent_check) >= PARENT_CHECK {
            last_parent_check = now;
            if parent.gone() {
                l.exit.get_or_insert(0);
            }
        }
    }
    l.be.destroy();
    let _ = l.be.prepare();
    ExitCode::from(l.exit.unwrap_or(0))
}

/// Pick the backend, see [`run`]. `CU_OVERLAY_BACKEND` = `layer-shell` or
/// `x11` forces one (an explicit `x11` also skips the XWayland and
/// compositing-manager checks, for tests on Xvfb); unset or `auto` tries
/// them in order.
pub const BACKEND_ENV: &str = "CU_OVERLAY_BACKEND";

// moved once into run_with, never stored
#[cfg(any(feature = "layer-shell", feature = "x11"))]
#[allow(clippy::large_enum_variant)]
enum Picked {
    #[cfg(feature = "layer-shell")]
    LayerShell(crate::backend::layer_shell::LayerShell),
    #[cfg(feature = "x11")]
    X11(crate::backend::x11::X11),
}

#[cfg(any(feature = "layer-shell", feature = "x11"))]
fn pick() -> Result<Picked, String> {
    let want = std::env::var(BACKEND_ENV).unwrap_or_default();
    let want = want.trim();
    let auto = want.is_empty() || want == "auto";
    if !auto && want != "layer-shell" && want != "x11" {
        return Err(format!(
            "{BACKEND_ENV}={want}: expected layer-shell, x11 or auto"
        ));
    }
    let mut errors: Vec<String> = Vec::new();
    if auto || want == "layer-shell" {
        #[cfg(feature = "layer-shell")]
        match crate::backend::layer_shell::LayerShell::connect() {
            Ok(be) => return Ok(Picked::LayerShell(be)),
            Err(e) => errors.push(e),
        }
        #[cfg(not(feature = "layer-shell"))]
        if !auto {
            errors.push("built without the layer-shell backend".into());
        }
    }
    if auto || want == "x11" {
        #[cfg(feature = "x11")]
        {
            use crate::backend::x11::{Options, X11};
            match X11::connect(Options::from_env(!auto)) {
                Ok(be) => return Ok(Picked::X11(be)),
                Err(e) => errors.push(e),
            }
        }
        #[cfg(not(feature = "x11"))]
        if !auto {
            errors.push("built without the x11 backend".into());
        }
    }
    Err(errors.join("; "))
}

/// Connect a backend and [`run_with`] it.
///
/// Backend selection (unless `CU_OVERLAY_BACKEND` forces one): the
/// layer-shell backend when the Wayland compositor advertises
/// `zwlr_layer_shell_v1`; else the x11 backend when `$DISPLAY` connects, the
/// server is XWayland and a compositing manager runs (GNOME/Mutter); a real
/// X11 session is never picked. When none fits, emits a fatal `error`
/// naming every reason and returns exit code 2 without creating anything.
#[cfg(any(feature = "layer-shell", feature = "x11"))]
pub fn run<R: AsFd, W: Write>(input: R, mut output: W) -> ExitCode {
    match pick() {
        #[cfg(feature = "layer-shell")]
        Ok(Picked::LayerShell(be)) => run_with(be, input, output),
        #[cfg(feature = "x11")]
        Ok(Picked::X11(be)) => run_with(be, input, output),
        Err(e) => {
            let _ = output.write_all(Event::error(e, true).line().as_bytes());
            let _ = output.flush();
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_and_bound() {
        let mut r = LineReader::default();
        assert!(r.push(b"{\"cmd\":").is_empty());
        let got = r.push(b"\"hide\"}\n\n{\"a\"");
        assert_eq!(
            got,
            vec![Ok("{\"cmd\":\"hide\"}".into()), Ok(String::new())]
        );
        assert_eq!(r.finish(), Some("{\"a\"".into()));
        assert_eq!(r.finish(), None);

        let mut r = LineReader::default();
        let long = vec![b'x'; MAX_LINE + 10];
        let got = r.push(&long);
        assert_eq!(got.len(), 1);
        assert!(got[0].is_err());
        // the rest of the long line is dropped, the next line is fine
        let got = r.push(b"yyy\nok\n");
        assert_eq!(got, vec![Ok("ok".into())]);
    }
}
