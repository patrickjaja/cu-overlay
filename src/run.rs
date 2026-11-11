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
use crate::protocol::{Event, MAX_LINE, PROTOCOL, parse_command};
use crate::sys::{self, Parent, ReadResult};

/// Parent check interval (as in cu-abort-watch).
const PARENT_CHECK: Duration = Duration::from_millis(500);
/// Glow pulse redraw interval when nothing else animates (alpha multiplier
/// only, no buffer redraw).
const PULSE_INTERVAL: Duration = Duration::from_millis(33);
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

    fn timeout(&self, now: Instant) -> Duration {
        let mut d = PARENT_CHECK;
        let mut at = |t: Instant| d = d.min(t.saturating_duration_since(now));
        if let Some(t) = self.ctl.next_deadline() {
            at(t);
        }
        if let Some(s) = &self.last_scene
            && self.be.ready_to_draw()
        {
            if s.pulse.is_some() {
                at(self.last_present + PULSE_INTERVAL);
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
        ctl: Controller::new(caps.alpha_modifier, now),
        be: backend,
        out: output,
        pending: HashMap::new(),
        next_token: 1,
        dirty: false,
        shown_due: false,
        last_present: now,
        last_scene: None,
        exit: None,
    };
    let ready = Event::Ready {
        protocol: PROTOCOL,
        backend: caps.name,
        alpha_modifier: caps.alpha_modifier,
        fractional_scale: caps.fractional_scale,
        badge: false,
        outputs: l.be.outputs(),
    };
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
            if (s.pulse.is_some() && since >= PULSE_INTERVAL)
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

/// Connect the layer-shell backend and [`run_with`] it. When there is no
/// Wayland display or no layer-shell, emits a fatal `error` event and
/// returns exit code 2 without creating anything.
#[cfg(feature = "layer-shell")]
pub fn run<R: AsFd, W: Write>(input: R, mut output: W) -> ExitCode {
    match crate::backend::layer_shell::LayerShell::connect() {
        Ok(be) => run_with(be, input, output),
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
