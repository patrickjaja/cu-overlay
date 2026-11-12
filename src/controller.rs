//! The protocol state machine, independent of any backend: what should be on
//! screen at a given instant. Time is passed in, so every rule here is unit
//! tested without a compositor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::anim::{self, Glide, PressFrame};
use crate::badge;
use crate::geom::OutputInfo;
use crate::protocol::{CapturePhase, CaptureScope, Command, Event, Look, Pose};

/// A `capture` hide that sees no `restore` ends by itself after this long,
/// so a bridge that died mid-screenshot never leaves the overlay invisible.
pub const CAPTURE_AUTO_RESTORE: Duration = Duration::from_millis(3000);

/// What the run loop must do after a command or a tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write this event.
    Emit(Event),
    /// Create the surfaces on this output (destroying any existing ones
    /// first). The loop emits `shown` once they are on screen.
    Create(String),
    /// Destroy the surfaces; the loop emits `hidden` once the compositor
    /// processed it.
    Destroy,
    /// Present the current scene, then ack `id` once the compositor
    /// processed it.
    Ack(u64),
    /// The scene changed: present it.
    Redraw,
    /// Exit with this code (after the events before it are written).
    Exit(i32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Vis {
    Hidden,
    /// Fading in from `from` (or fully shown once the fade is over).
    Shown {
        from: f64,
        since: Instant,
    },
    /// Fading out from `from`; destroyed when the fade ends.
    Hiding {
        from: f64,
        since: Instant,
    },
}

/// The drawn arrow for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrowDraw {
    /// Global logical hotspot position, tilt and lift.
    pub pose: Pose,
    /// Press bump (scale around the hotspot).
    pub scale: f64,
}

/// The press ring for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PressDraw {
    /// Global logical centre.
    pub x: f64,
    pub y: f64,
    pub frame: PressFrame,
}

/// The badge for one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct BadgeDraw {
    /// Milliseconds since the flash started (see [`badge::pose`]).
    pub ms: f64,
    pub text: Arc<str>,
}

/// Everything a backend needs to draw one frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub look: Look,
    /// Overall opacity: the show/hide fade times the capture hide (0).
    pub opacity: f64,
    /// Glow pulse multiplier (`Some` only while the glow pulses; the glow is
    /// then drawn at the pulse's peak).
    pub pulse: Option<f64>,
    pub arrow: Option<ArrowDraw>,
    pub press: Option<PressDraw>,
    /// The badge flash while it runs. It is not a cursor: a `cursor` capture
    /// hide leaves it, an `all` hide takes it with the rest (`opacity`).
    pub badge: Option<BadgeDraw>,
    /// A frame-paced animation is running (glide, press, fade).
    pub animating: bool,
}

pub struct Controller {
    /// The backend can change opacity without redrawing (alpha modifier):
    /// fades and the pulse run. Without it show/hide are instant and the
    /// glow is static.
    fades: bool,
    look: Look,
    reduced_motion: bool,
    output: Option<String>,
    vis: Vis,
    /// Where the arrow rests (global); `None` until placed.
    rest: Option<Pose>,
    glide: Option<(Glide, Instant)>,
    press: Option<(f64, f64, Instant)>,
    /// A capture hide in force: its scope and auto-restore deadline.
    capture: Option<(CaptureScope, Instant)>,
    pulse_epoch: Instant,
    /// The running badge flash: its text and start.
    badge: Option<(Arc<str>, Instant)>,
}

fn ms_since(now: Instant, t: Instant) -> f64 {
    now.saturating_duration_since(t).as_secs_f64() * 1000.0
}

impl Controller {
    pub fn new(fades: bool, now: Instant) -> Self {
        Self {
            fades,
            look: Look::Glow,
            reduced_motion: false,
            output: None,
            vis: Vis::Hidden,
            rest: None,
            glide: None,
            press: None,
            capture: None,
            pulse_epoch: now,
            badge: None,
        }
    }

    pub fn output(&self) -> Option<&str> {
        self.output.as_deref()
    }

    pub fn visible(&self) -> bool {
        self.vis != Vis::Hidden
    }

    fn fade_opacity(&self, now: Instant) -> f64 {
        match self.vis {
            Vis::Hidden => 0.0,
            Vis::Shown { from, since } => anim::fade(from, 1.0, ms_since(now, since)),
            Vis::Hiding { from, since } => anim::fade(from, 0.0, ms_since(now, since)),
        }
    }

    fn pose(&self, now: Instant) -> Option<Pose> {
        match &self.glide {
            Some((g, start)) => Some(g.pose_at(ms_since(now, *start))),
            None => self.rest,
        }
    }

    fn unknown_output(name: &str) -> Vec<Action> {
        vec![
            Action::Emit(Event::error(format!("unknown output: {name}"), true)),
            Action::Exit(2),
        ]
    }

    /// Apply one command.
    ///
    /// Arrow points (`jump`, `press`, glide frames, a `show` arrow) that lie
    /// off the shown output while the drawn cursor is on (shown, rim look)
    /// are accepted and kept, but not drawn; one non-fatal
    /// `arrow outside output <name>` error is emitted per command.
    pub fn handle(&mut self, cmd: Command, now: Instant, outputs: &[OutputInfo]) -> Vec<Action> {
        let points: Vec<(f64, f64)> = match &cmd {
            Command::Jump { x, y } | Command::Press { x, y } => vec![(*x, *y)],
            Command::Glide { frames, .. } => frames.iter().map(|f| (f.x, f.y)).collect(),
            Command::Show(s) => s.arrow.into_iter().collect(),
            _ => Vec::new(),
        };
        let mut actions = self.apply(cmd, now, outputs);
        if actions.iter().any(|a| matches!(a, Action::Exit(_))) {
            return actions;
        }
        if self.visible()
            && self.look == Look::Rim
            && let Some(out) = self
                .output
                .as_deref()
                .and_then(|n| outputs.iter().find(|o| o.name == n))
            && points.iter().any(|&(x, y)| !out.contains(x, y))
        {
            actions.push(Action::Emit(Event::error(
                format!("arrow outside output {}", out.name),
                false,
            )));
        }
        actions
    }

    fn apply(&mut self, cmd: Command, now: Instant, outputs: &[OutputInfo]) -> Vec<Action> {
        let known = |name: &str| outputs.iter().any(|o| o.name == name);
        match cmd {
            Command::Show(s) => {
                if !known(&s.output) {
                    return Self::unknown_output(&s.output);
                }
                self.look = s.look;
                self.reduced_motion = s.reduced_motion;
                // like upstream's page: every show restarts the flash or, with
                // `badge:false`, clears it; reduced motion never shows it
                self.badge = (s.badge && !s.reduced_motion).then(|| {
                    let text = s.badge_text.as_deref().unwrap_or(badge::DEFAULT_TEXT);
                    (Arc::from(text), now)
                });
                self.glide = None;
                self.rest = s.arrow.map(|(x, y)| Pose::at(x, y));
                let start = if self.fades { 0.0 } else { 1.0 };
                let mut out = Vec::new();
                match self.vis {
                    Vis::Hidden => {
                        self.pulse_epoch = now;
                        self.vis = Vis::Shown {
                            from: start,
                            since: now,
                        };
                        out.push(Action::Create(s.output.clone()));
                    }
                    _ if self.output.as_deref() != Some(&s.output) => {
                        self.vis = Vis::Shown {
                            from: start,
                            since: now,
                        };
                        out.push(Action::Create(s.output.clone()));
                    }
                    Vis::Hiding { .. } => {
                        let from = self.fade_opacity(now);
                        self.vis = Vis::Shown { from, since: now };
                    }
                    Vis::Shown { .. } => {}
                }
                self.output = Some(s.output);
                out.push(Action::Redraw);
                out
            }
            Command::Move { output } => {
                if !known(&output) {
                    // not fatal: keep showing where we are
                    return vec![Action::Emit(Event::error(
                        format!("move: unknown output: {output}"),
                        false,
                    ))];
                }
                if self.output.as_deref() == Some(&output) {
                    return vec![];
                }
                self.output = Some(output.clone());
                if self.visible() {
                    let start = if self.fades { 0.0 } else { 1.0 };
                    self.vis = Vis::Shown {
                        from: start,
                        since: now,
                    };
                    vec![Action::Create(output), Action::Redraw]
                } else {
                    vec![]
                }
            }
            Command::Glide { frames, frame_ms } => {
                let g = Glide { frames, frame_ms };
                self.rest = Some(g.rest());
                self.glide = if self.reduced_motion || g.duration_ms() <= 0.0 {
                    None
                } else {
                    Some((g, now))
                };
                vec![Action::Redraw]
            }
            Command::Jump { x, y } => {
                self.glide = None;
                self.rest = Some(Pose::at(x, y));
                vec![Action::Redraw]
            }
            Command::Press { x, y } => {
                self.glide = None;
                self.rest = Some(Pose::at(x, y));
                self.press = Some((x, y, now));
                vec![Action::Redraw]
            }
            Command::Look(l) => {
                self.look = l;
                vec![Action::Redraw]
            }
            Command::Hide => match self.vis {
                Vis::Hidden => vec![Action::Emit(Event::Hidden)],
                Vis::Hiding { .. } => vec![],
                Vis::Shown { .. } if self.fades => {
                    let from = self.fade_opacity(now);
                    self.vis = Vis::Hiding { from, since: now };
                    vec![Action::Redraw]
                }
                Vis::Shown { .. } => {
                    self.vis = Vis::Hidden;
                    self.press = None;
                    self.badge = None;
                    vec![Action::Destroy]
                }
            },
            Command::Capture { phase, id, scope } => {
                self.capture = match phase {
                    // not nested: a second hide widens to the larger scope
                    // and restarts the auto-restore clock; one restore ends it
                    CapturePhase::Hide => {
                        let scope = match self.capture {
                            Some((CaptureScope::All, _)) => CaptureScope::All,
                            _ => scope,
                        };
                        Some((scope, now + CAPTURE_AUTO_RESTORE))
                    }
                    CapturePhase::Restore => None,
                };
                vec![Action::Redraw, Action::Ack(id)]
            }
        }
    }

    /// Time-driven transitions: the end of a hide fade, a capture hide that
    /// timed out, finished glides and presses.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        if let Vis::Hiding { since, .. } = self.vis
            && ms_since(now, since) >= anim::FADE_MS
        {
            self.vis = Vis::Hidden;
            self.press = None;
            self.badge = None;
            out.push(Action::Destroy);
        }
        if let Some((_, start)) = &self.badge
            && ms_since(now, *start) >= badge::FLASH_MS
        {
            self.badge = None;
            out.push(Action::Redraw);
        }
        if let Some((g, start)) = &self.glide
            && ms_since(now, *start) >= g.duration_ms()
        {
            self.glide = None;
            out.push(Action::Redraw);
        }
        if let Some((_, _, t)) = self.press
            && anim::press_frame(ms_since(now, t), self.reduced_motion).is_none()
        {
            self.press = None;
            out.push(Action::Redraw);
        }
        if let Some((_, until)) = self.capture
            && now >= until
        {
            self.capture = None;
            out.push(Action::Emit(Event::error(
                format!(
                    "capture: no restore within {} ms, overlay restored",
                    CAPTURE_AUTO_RESTORE.as_millis()
                ),
                false,
            )));
            out.push(Action::Redraw);
        }
        out
    }

    /// The next instant `tick` has work, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut min = |t: Instant| d = Some(d.map_or(t, |x| x.min(t)));
        if let Vis::Hiding { since, .. } = self.vis {
            min(since + Duration::from_secs_f64(anim::FADE_MS / 1000.0));
        }
        if let Some((g, start)) = &self.glide {
            min(*start + Duration::from_secs_f64(g.duration_ms() / 1000.0));
        }
        if let Some((_, _, t)) = self.press {
            let ms = if self.reduced_motion {
                anim::RING_REDUCED_MS
            } else {
                anim::RING_MS
            };
            min(t + Duration::from_secs_f64(ms / 1000.0));
        }
        if let Some((_, t)) = self.capture {
            min(t);
        }
        if let Some((_, start)) = &self.badge {
            min(*start + Duration::from_secs_f64(badge::FLASH_MS / 1000.0));
        }
        d
    }

    /// The display server took the surfaces away (output unplugged): the
    /// overlay is hidden; a later `show` creates new ones.
    pub fn surfaces_lost(&mut self) {
        self.vis = Vis::Hidden;
        self.press = None;
        self.glide = None;
        self.badge = None;
    }

    /// The scope of the capture hide in force, if any.
    pub fn capture_scope(&self) -> Option<CaptureScope> {
        self.capture.map(|(s, _)| s)
    }

    pub fn capture_hidden(&self) -> bool {
        self.capture_scope() == Some(CaptureScope::All)
    }

    /// What to draw now; `None` while hidden.
    pub fn scene(&self, now: Instant) -> Option<Scene> {
        if self.vis == Vis::Hidden {
            return None;
        }
        let fading = match self.vis {
            Vis::Shown { from, since } => from < 1.0 && ms_since(now, since) < anim::FADE_MS,
            Vis::Hiding { .. } => true,
            Vis::Hidden => false,
        };
        let opacity = if self.capture_hidden() {
            0.0
        } else {
            self.fade_opacity(now)
        };
        let press = self.press.and_then(|(x, y, t)| {
            anim::press_frame(ms_since(now, t), self.reduced_motion).map(|frame| PressDraw {
                x,
                y,
                frame,
            })
        });
        // a cursor-scope capture hide drops the arrow and ring from the
        // frame; their clocks keep running, so restore shows the current pose
        // and a ring only while its animation still runs
        let cursor_off = self.capture.is_some();
        let arrow = match (self.look, self.pose(now)) {
            _ if cursor_off => None,
            (Look::Rim, Some(pose)) => Some(ArrowDraw {
                pose,
                scale: press.map_or(1.0, |p| p.frame.arrow_scale),
            }),
            _ => None,
        };
        let badge = self.badge.as_ref().and_then(|(text, start)| {
            let ms = ms_since(now, *start);
            (ms < badge::FLASH_MS).then(|| BadgeDraw {
                ms,
                text: text.clone(),
            })
        });
        let badge_moving = badge.as_ref().is_some_and(|b| badge::moving(b.ms));
        let pulse = (self.look == Look::Glow && !self.reduced_motion && self.fades)
            .then(|| anim::pulse(ms_since(now, self.pulse_epoch)));
        Some(Scene {
            look: self.look,
            opacity,
            pulse,
            arrow,
            // the ring belongs to the drawn cursor (rim look only)
            press: if self.look == Look::Rim && !cursor_off {
                press
            } else {
                None
            },
            badge,
            animating: fading
                || badge_moving
                || (!cursor_off && (self.glide.is_some() || press.is_some())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Show, parse_command};

    fn outs() -> Vec<OutputInfo> {
        vec![
            OutputInfo {
                name: "A".into(),
                x: 0,
                y: 0,
                width: 1280,
                height: 800,
                scale: 1.0,
            },
            OutputInfo {
                name: "B".into(),
                x: 1280,
                y: 0,
                width: 853,
                height: 533,
                scale: 1.5,
            },
        ]
    }

    fn cmd(s: &str) -> Command {
        parse_command(s).unwrap()
    }

    fn ms(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn show_fades_in_and_hide_fades_out_then_destroys() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        let a = c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"glow"}"#),
            t0,
            &outs(),
        );
        assert_eq!(a, vec![Action::Create("A".into()), Action::Redraw]);
        let s = c.scene(t0).unwrap();
        assert_eq!(s.opacity, 0.0);
        assert!(s.animating);
        assert!(s.pulse.is_some());
        assert!(s.arrow.is_none());
        assert_eq!(c.scene(ms(t0, 300)).unwrap().opacity, 1.0);
        assert!(!c.scene(ms(t0, 300)).unwrap().animating);

        let t1 = ms(t0, 1000);
        assert_eq!(c.handle(Command::Hide, t1, &outs()), vec![Action::Redraw]);
        assert!(c.tick(ms(t0, 1200)).is_empty());
        assert!(c.scene(ms(t0, 1150)).unwrap().opacity < 1.0);
        assert_eq!(c.next_deadline(), Some(ms(t0, 1300)));
        assert_eq!(c.tick(ms(t0, 1300)), vec![Action::Destroy]);
        assert!(c.scene(ms(t0, 1300)).is_none());
        // hide while hidden answers at once
        assert_eq!(
            c.handle(Command::Hide, ms(t0, 1400), &outs()),
            vec![Action::Emit(Event::Hidden)]
        );
    }

    #[test]
    fn without_alpha_modifier_show_and_hide_are_instant() {
        let t0 = Instant::now();
        let mut c = Controller::new(false, t0);
        c.handle(cmd(r#"{"cmd":"show","output":"A"}"#), t0, &outs());
        let s = c.scene(t0).unwrap();
        assert_eq!(s.opacity, 1.0);
        assert_eq!(s.pulse, None, "static glow without alpha modifier");
        assert!(!s.animating);
        assert_eq!(c.handle(Command::Hide, t0, &outs()), vec![Action::Destroy]);
    }

    #[test]
    fn unknown_output_is_fatal() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        let a = c.handle(cmd(r#"{"cmd":"show","output":"Z"}"#), t0, &outs());
        assert_eq!(a[1], Action::Exit(2));
        assert!(matches!(
            &a[0],
            Action::Emit(Event::Error { fatal: true, .. })
        ));
    }

    fn errors(a: &[Action]) -> Vec<String> {
        a.iter()
            .filter_map(|a| match a {
                Action::Emit(Event::Error { message, fatal }) => {
                    assert!(!fatal);
                    Some(message.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn arrow_off_the_shown_output_is_kept_and_reported_once() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim"}"#),
            t0,
            &outs(),
        );
        // B starts at x = 1280
        let a = c.handle(cmd(r#"{"cmd":"jump","x":1500,"y":100}"#), t0, &outs());
        assert_eq!(errors(&a), vec!["arrow outside output A".to_string()]);
        assert_eq!(
            c.scene(t0).unwrap().arrow.unwrap().pose,
            Pose::at(1500.0, 100.0)
        );
        let a = c.handle(cmd(r#"{"cmd":"press","x":1500,"y":100}"#), t0, &outs());
        assert_eq!(errors(&a).len(), 1);
        // a glide leaving the output: one error for the whole command
        let a = c.handle(
            cmd(r#"{"cmd":"glide","frames":[[1200,100],[1270,100],[1300,100],[1400,100]]}"#),
            t0,
            &outs(),
        );
        assert_eq!(errors(&a).len(), 1);
        // in bounds: no error
        let a = c.handle(cmd(r#"{"cmd":"jump","x":1279,"y":799}"#), t0, &outs());
        assert!(errors(&a).is_empty());
        // glow look or hidden: nothing to report
        c.handle(cmd(r#"{"cmd":"look","look":"glow"}"#), t0, &outs());
        assert!(errors(&c.handle(cmd(r#"{"cmd":"jump","x":1500,"y":1}"#), t0, &outs())).is_empty());
    }

    #[test]
    fn move_keeps_the_arrow_pose_for_the_new_output() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim"}"#),
            t0,
            &outs(),
        );
        c.handle(cmd(r#"{"cmd":"jump","x":1680,"y":300}"#), t0, &outs());
        let a = c.handle(cmd(r#"{"cmd":"move","output":"B"}"#), t0, &outs());
        assert_eq!(a, vec![Action::Create("B".into()), Action::Redraw]);
        let s = c.scene(ms(t0, 400)).unwrap();
        assert_eq!(s.arrow.unwrap().pose, Pose::at(1680.0, 300.0));
        assert_eq!(s.opacity, 1.0);
        // and the point is on B, so no error on the next in-bounds command
        assert!(
            errors(&c.handle(cmd(r#"{"cmd":"jump","x":1700,"y":300}"#), t0, &outs())).is_empty()
        );
    }

    #[test]
    fn move_to_unknown_output_is_not_fatal() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim"}"#),
            t0,
            &outs(),
        );
        let a = c.handle(cmd(r#"{"cmd":"move","output":"Z"}"#), t0, &outs());
        assert_eq!(a.len(), 1);
        assert!(matches!(
            &a[0],
            Action::Emit(Event::Error { fatal: false, .. })
        ));
        assert_eq!(c.output(), Some("A"));
        assert!(c.scene(t0).is_some(), "still shown");
    }

    #[test]
    fn move_recreates_on_the_new_output() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim"}"#),
            t0,
            &outs(),
        );
        assert!(
            c.handle(cmd(r#"{"cmd":"move","output":"A"}"#), t0, &outs())
                .is_empty()
        );
        assert_eq!(
            c.handle(cmd(r#"{"cmd":"move","output":"B"}"#), t0, &outs()),
            vec![Action::Create("B".into()), Action::Redraw]
        );
        assert_eq!(c.output(), Some("B"));
    }

    #[test]
    fn rim_arrow_only_when_placed() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            Command::Show(Show {
                output: "A".into(),
                look: Look::Rim,
                badge: false,
                badge_text: None,
                reduced_motion: false,
                arrow: None,
            }),
            t0,
            &outs(),
        );
        let s = c.scene(t0).unwrap();
        assert!(s.arrow.is_none());
        assert_eq!(s.pulse, None, "the rim does not pulse");
        c.handle(cmd(r#"{"cmd":"jump","x":100,"y":50}"#), t0, &outs());
        assert_eq!(
            c.scene(t0).unwrap().arrow.unwrap().pose,
            Pose::at(100.0, 50.0)
        );
        // glow look hides the arrow but keeps its pose
        c.handle(cmd(r#"{"cmd":"look","look":"glow"}"#), t0, &outs());
        assert!(c.scene(t0).unwrap().arrow.is_none());
        c.handle(cmd(r#"{"cmd":"look","look":"rim"}"#), t0, &outs());
        assert!(c.scene(t0).unwrap().arrow.is_some());
    }

    #[test]
    fn glide_plays_then_rests() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","arrow":{"x":0,"y":0}}"#),
            t0,
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"glide","frames":[[0,0,0,0],[16,0,2,0.5],[32,0,0,0]],"frameMs":16}"#),
            t0,
            &outs(),
        );
        let s = c.scene(ms(t0, 8)).unwrap();
        assert!(s.animating);
        let a = s.arrow.unwrap();
        assert!((a.pose.x - 8.0).abs() < 1e-6 && (a.pose.deg - 1.0).abs() < 1e-6);
        assert_eq!(c.next_deadline(), Some(ms(t0, 32)));
        assert_eq!(c.tick(ms(t0, 32)), vec![Action::Redraw]);
        let s = c.scene(ms(t0, 400)).unwrap();
        assert!(!s.animating);
        assert_eq!(s.arrow.unwrap().pose, Pose::at(32.0, 0.0));
    }

    #[test]
    fn reduced_motion_glide_jumps() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","reducedMotion":true}"#),
            t0,
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"glide","frames":[[0,0],[5,5],[9,9]]}"#),
            t0,
            &outs(),
        );
        assert_eq!(
            c.scene(ms(t0, 1)).unwrap().arrow.unwrap().pose,
            Pose::at(9.0, 9.0)
        );
        assert_eq!(c.scene(t0).unwrap().pulse, None);
    }

    #[test]
    fn press_snaps_and_rings() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim"}"#),
            t0,
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"glide","frames":[[0,0],[50,50],[100,100]]}"#),
            t0,
            &outs(),
        );
        c.handle(cmd(r#"{"cmd":"press","x":10,"y":20}"#), ms(t0, 5), &outs());
        let s = c.scene(ms(t0, 105)).unwrap();
        assert_eq!(s.arrow.unwrap().pose, Pose::at(10.0, 20.0));
        assert!((s.arrow.unwrap().scale - 0.87).abs() < 1e-9);
        let p = s.press.unwrap();
        assert_eq!((p.x, p.y), (10.0, 20.0));
        assert!(s.animating);
        // a glide after the press moves the arrow, the ring stays put
        c.handle(
            cmd(r#"{"cmd":"glide","frames":[[10,20],[60,20],[90,20]]}"#),
            ms(t0, 200),
            &outs(),
        );
        let s = c.scene(ms(t0, 216)).unwrap();
        assert_eq!(s.press.unwrap().x, 10.0);
        assert!((s.arrow.unwrap().pose.x - 60.0).abs() < 1e-6);
        assert_eq!(c.tick(ms(t0, 1105)), vec![Action::Redraw, Action::Redraw]);
        assert!(c.scene(ms(t0, 1105)).unwrap().press.is_none());
    }

    #[test]
    fn capture_hide_restore_and_auto_restore() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim"}"#),
            t0,
            &outs(),
        );
        let a = c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":1}"#),
            ms(t0, 500),
            &outs(),
        );
        assert_eq!(a, vec![Action::Redraw, Action::Ack(1)]);
        assert_eq!(c.scene(ms(t0, 600)).unwrap().opacity, 0.0);
        let a = c.handle(
            cmd(r#"{"cmd":"capture","phase":"restore","id":2}"#),
            ms(t0, 700),
            &outs(),
        );
        assert_eq!(a, vec![Action::Redraw, Action::Ack(2)]);
        assert_eq!(c.scene(ms(t0, 700)).unwrap().opacity, 1.0);

        // no restore: back on its own after the timeout, with a non-fatal error
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":3}"#),
            ms(t0, 1000),
            &outs(),
        );
        assert_eq!(c.next_deadline(), Some(ms(t0, 4000)));
        assert!(c.tick(ms(t0, 3999)).is_empty());
        assert_eq!(c.scene(ms(t0, 3999)).unwrap().opacity, 0.0);
        let a = c.tick(ms(t0, 4000));
        assert!(matches!(
            &a[0],
            Action::Emit(Event::Error { fatal: false, .. })
        ));
        assert_eq!(a[1], Action::Redraw);
        assert_eq!(c.scene(ms(t0, 4000)).unwrap().opacity, 1.0);
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn capture_cursor_scope_keeps_the_rim_and_drops_arrow_and_ring() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","arrow":{"x":0,"y":0}}"#),
            t0,
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"press","x":10,"y":10}"#),
            ms(t0, 400),
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"glide","frames":[[10,10],[110,10],[210,10],[310,10]],"frameMs":100}"#),
            ms(t0, 450),
            &outs(),
        );
        let a = c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":1,"scope":"cursor"}"#),
            ms(t0, 500),
            &outs(),
        );
        assert_eq!(a, vec![Action::Redraw, Action::Ack(1)]);
        let s = c.scene(ms(t0, 550)).unwrap();
        assert_eq!(s.opacity, 1.0, "rim untouched");
        assert!(s.arrow.is_none() && s.press.is_none());
        assert!(
            !s.animating,
            "nothing to animate while the cursor is hidden"
        );
        assert!(!c.capture_hidden());
        assert_eq!(c.capture_scope(), Some(CaptureScope::Cursor));
        // idempotent: a second hide acks again, no nesting
        let a = c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":2,"scope":"cursor"}"#),
            ms(t0, 600),
            &outs(),
        );
        assert_eq!(a, vec![Action::Redraw, Action::Ack(2)]);
        // the glide's clock kept running: restore shows the CURRENT pose
        c.tick(ms(t0, 700));
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"restore","id":3}"#),
            ms(t0, 700),
            &outs(),
        );
        let s = c.scene(ms(t0, 700)).unwrap();
        let p = s.arrow.unwrap().pose;
        assert!(
            (p.x - 260.0).abs() < 1e-6,
            "pose at 250 ms into the glide: {p:?}"
        );
        // the press (400 ms + 1100 ms) still runs: the ring comes back mid-way
        let ring = s.press.unwrap();
        assert!(ring.frame.ring_scale > 1.0);
    }

    #[test]
    fn capture_restore_does_not_replay_a_finished_ring() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","arrow":{"x":5,"y":5}}"#),
            t0,
            &outs(),
        );
        c.handle(cmd(r#"{"cmd":"press","x":5,"y":5}"#), t0, &outs());
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":1,"scope":"cursor"}"#),
            ms(t0, 100),
            &outs(),
        );
        c.tick(ms(t0, 1200)); // the ring ended while hidden
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"restore","id":2}"#),
            ms(t0, 1300),
            &outs(),
        );
        let s = c.scene(ms(t0, 1300)).unwrap();
        assert!(s.press.is_none(), "finished ring not replayed");
        assert_eq!(s.arrow.unwrap().pose, Pose::at(5.0, 5.0));
        // even without the tick, a ring past its end is not drawn
        c.handle(cmd(r#"{"cmd":"press","x":5,"y":5}"#), ms(t0, 2000), &outs());
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":3,"scope":"cursor"}"#),
            ms(t0, 2000),
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"restore","id":4}"#),
            ms(t0, 3200),
            &outs(),
        );
        assert!(c.scene(ms(t0, 3200)).unwrap().press.is_none());
    }

    #[test]
    fn capture_cursor_scope_auto_restores_and_widens_to_all() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","arrow":{"x":5,"y":5}}"#),
            t0,
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":1,"scope":"cursor"}"#),
            ms(t0, 500),
            &outs(),
        );
        assert_eq!(c.next_deadline(), Some(ms(t0, 3500)));
        let a = c.tick(ms(t0, 3500));
        assert!(matches!(
            &a[0],
            Action::Emit(Event::Error { fatal: false, .. })
        ));
        assert!(c.scene(ms(t0, 3500)).unwrap().arrow.is_some());
        // cursor then all: all wins; all then cursor: stays all
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":2,"scope":"cursor"}"#),
            ms(t0, 4000),
            &outs(),
        );
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":3}"#),
            ms(t0, 4000),
            &outs(),
        );
        assert_eq!(c.capture_scope(), Some(CaptureScope::All));
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":4,"scope":"cursor"}"#),
            ms(t0, 4000),
            &outs(),
        );
        assert_eq!(c.capture_scope(), Some(CaptureScope::All));
        assert_eq!(c.scene(ms(t0, 4000)).unwrap().opacity, 0.0);
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"restore","id":5}"#),
            ms(t0, 4000),
            &outs(),
        );
        assert_eq!(c.capture_scope(), None);
        // a cursor hide with nothing of the cursor visible (glow look) still acks
        c.handle(
            cmd(r#"{"cmd":"look","look":"glow"}"#),
            ms(t0, 4000),
            &outs(),
        );
        let a = c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":6,"scope":"cursor"}"#),
            ms(t0, 4000),
            &outs(),
        );
        assert_eq!(a, vec![Action::Redraw, Action::Ack(6)]);
    }

    #[test]
    fn capture_while_hidden_still_acks_and_show_stays_invisible() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        let a = c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":9}"#),
            t0,
            &outs(),
        );
        assert_eq!(a, vec![Action::Redraw, Action::Ack(9)]);
        c.handle(cmd(r#"{"cmd":"show","output":"A"}"#), t0, &outs());
        assert_eq!(c.scene(ms(t0, 400)).unwrap().opacity, 0.0);
    }

    #[test]
    fn show_during_hide_fade_reverses() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(cmd(r#"{"cmd":"show","output":"A"}"#), t0, &outs());
        c.handle(Command::Hide, ms(t0, 1000), &outs());
        let a = c.handle(cmd(r#"{"cmd":"show","output":"A"}"#), ms(t0, 1150), &outs());
        assert_eq!(a, vec![Action::Redraw], "no new surface");
        assert!(c.tick(ms(t0, 1400)).is_empty(), "no destroy");
        assert_eq!(c.scene(ms(t0, 1500)).unwrap().opacity, 1.0);
    }

    fn badge_ms(c: &Controller, now: Instant) -> Option<f64> {
        c.scene(now).unwrap().badge.map(|b| b.ms)
    }

    #[test]
    fn badge_flashes_for_six_seconds_then_stops_needing_ticks() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        // protocol-1 form: badge true only, built-in text
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true}"#),
            t0,
            &outs(),
        );
        let b = c.scene(ms(t0, 1000)).unwrap().badge.unwrap();
        assert_eq!(b.ms, 1000.0);
        assert_eq!(&*b.text, crate::badge::DEFAULT_TEXT);
        // the end of the flash is a deadline and a final redraw (blanking)
        assert_eq!(c.next_deadline(), Some(ms(t0, 6000)));
        assert!(c.tick(ms(t0, 5999)).is_empty());
        assert!(badge_ms(&c, ms(t0, 5999)).is_some());
        assert_eq!(c.tick(ms(t0, 6000)), vec![Action::Redraw]);
        assert_eq!(badge_ms(&c, ms(t0, 6000)), None);
        assert_eq!(c.next_deadline(), None);
        // a static scene afterwards: nothing animates, no pulse timer
        let s = c.scene(ms(t0, 7000)).unwrap();
        assert!(!s.animating && s.badge.is_none());
    }

    #[test]
    fn badge_text_override_restart_and_clear() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","badge":true,"badgeText":"Claude nutzt deinen Computer"}"#),
            t0,
            &outs(),
        );
        let b = c.scene(ms(t0, 10)).unwrap().badge.unwrap();
        assert_eq!(&*b.text, "Claude nutzt deinen Computer");
        // every show with a badge restarts the flash
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","badge":true}"#),
            ms(t0, 2000),
            &outs(),
        );
        assert_eq!(badge_ms(&c, ms(t0, 2500)), Some(500.0));
        // a show without it clears a running flash (as upstream's page does)
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","badge":false}"#),
            ms(t0, 3000),
            &outs(),
        );
        assert_eq!(badge_ms(&c, ms(t0, 3001)), None);
        assert_eq!(c.next_deadline(), None);
        // text without badge:true draws nothing
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badgeText":"x"}"#),
            ms(t0, 3100),
            &outs(),
        );
        assert_eq!(badge_ms(&c, ms(t0, 3200)), None);
    }

    #[test]
    fn badge_is_hidden_under_reduced_motion() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true,"reducedMotion":true}"#),
            t0,
            &outs(),
        );
        assert_eq!(badge_ms(&c, ms(t0, 500)), None);
        assert_eq!(c.next_deadline(), None);
        // switching reduced motion on while a flash runs cancels it
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true}"#),
            ms(t0, 600),
            &outs(),
        );
        assert!(badge_ms(&c, ms(t0, 700)).is_some());
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true,"reducedMotion":true}"#),
            ms(t0, 800),
            &outs(),
        );
        assert_eq!(badge_ms(&c, ms(t0, 900)), None);
    }

    #[test]
    fn badge_needs_frames_only_while_it_moves() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true}"#),
            t0,
            &outs(),
        );
        // flash-in (and the layer's own fade): frame-paced
        assert!(c.scene(ms(t0, 100)).unwrap().animating);
        // the hold: the dot's timer only
        let hold = c.scene(ms(t0, 2000)).unwrap();
        assert!(!hold.animating && hold.badge.is_some());
        // the slide to the corner: frame-paced again
        assert!(c.scene(ms(t0, 5500)).unwrap().animating);
    }

    #[test]
    fn badge_follows_capture_scopes_not_cursor() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","look":"rim","badge":true,"arrow":{"x":5,"y":5}}"#),
            t0,
            &outs(),
        );
        // scope cursor: arrow gone, badge and opacity untouched
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":1,"scope":"cursor"}"#),
            ms(t0, 1000),
            &outs(),
        );
        let s = c.scene(ms(t0, 1100)).unwrap();
        assert!(s.arrow.is_none() && s.badge.is_some() && s.opacity == 1.0);
        // scope all: the scene's opacity is 0, which the backend applies to
        // the badge (it blanks the subsurface); the flash clock keeps running
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"hide","id":2,"scope":"all"}"#),
            ms(t0, 1200),
            &outs(),
        );
        let s = c.scene(ms(t0, 1300)).unwrap();
        assert_eq!(s.opacity, 0.0);
        assert_eq!(s.badge.unwrap().ms, 1300.0);
        c.handle(
            cmd(r#"{"cmd":"capture","phase":"restore","id":3}"#),
            ms(t0, 1400),
            &outs(),
        );
        let s = c.scene(ms(t0, 1500)).unwrap();
        assert!(s.opacity > 0.99 && s.badge.is_some());
    }

    #[test]
    fn badge_goes_with_the_surfaces() {
        let t0 = Instant::now();
        // instant hide (no alpha modifier)
        let mut c = Controller::new(false, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true}"#),
            t0,
            &outs(),
        );
        c.handle(cmd(r#"{"cmd":"hide"}"#), ms(t0, 100), &outs());
        assert!(c.scene(ms(t0, 100)).is_none());
        c.handle(cmd(r#"{"cmd":"show","output":"A"}"#), ms(t0, 200), &outs());
        assert_eq!(badge_ms(&c, ms(t0, 300)), None);
        // fading hide: the badge fades with the layer, then is dropped
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true}"#),
            t0,
            &outs(),
        );
        c.handle(cmd(r#"{"cmd":"hide"}"#), ms(t0, 1000), &outs());
        assert!(badge_ms(&c, ms(t0, 1100)).is_some());
        assert!(c.tick(ms(t0, 1300)).contains(&Action::Destroy));
        // lost surfaces
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true}"#),
            t0,
            &outs(),
        );
        c.surfaces_lost();
        c.handle(cmd(r#"{"cmd":"show","output":"A"}"#), ms(t0, 100), &outs());
        assert_eq!(badge_ms(&c, ms(t0, 200)), None);
    }

    #[test]
    fn badge_continues_across_a_move() {
        let t0 = Instant::now();
        let mut c = Controller::new(true, t0);
        c.handle(
            cmd(r#"{"cmd":"show","output":"A","badge":true}"#),
            t0,
            &outs(),
        );
        c.handle(cmd(r#"{"cmd":"move","output":"B"}"#), ms(t0, 2000), &outs());
        assert_eq!(badge_ms(&c, ms(t0, 2500)), Some(2500.0));
    }
}
