//! The wire protocol: JSON lines on stdin (commands) and stdout (events).
//! Protocol version [`PROTOCOL`]. See README.md for the contract.

use serde::{Deserialize, Serialize};

use crate::geom::OutputInfo;

/// Protocol version reported in `ready`.
pub const PROTOCOL: u32 = 1;

/// Longest accepted command line in bytes. Longer lines are reported as a
/// non-fatal error and dropped up to the next newline.
pub const MAX_LINE: usize = 256 * 1024;

/// Most frames one `glide` may carry (upstream sends about 80).
pub const MAX_GLIDE_FRAMES: usize = 4096;

/// What the overlay draws on its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Look {
    /// Upstream's classic pulsing glow (no drawn cursor).
    #[default]
    Glow,
    /// The 2 px rim of upstream's own-cursor mode, plus the drawn arrow.
    Rim,
}

/// One drawn-cursor pose: global logical position, tilt in degrees, lift
/// (0..1, grows the arrow and its shadow while it moves).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub x: f64,
    pub y: f64,
    pub deg: f64,
    pub lift: f64,
}

impl Pose {
    pub fn at(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            deg: 0.0,
            lift: 0.0,
        }
    }
}

/// What a capture hide removes from the composited image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureScope {
    /// Everything (rim or glow included) fully transparent.
    #[default]
    All,
    /// Only the drawn cursor (arrow and press ring); the rim or glow stays.
    Cursor,
}

/// The scopes `capture` accepts, advertised in `ready`.
pub const CAPTURE_SCOPES: &[&str] = &["all", "cursor"];

/// A capture command's phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapturePhase {
    Hide,
    Restore,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Show {
    pub output: String,
    pub look: Look,
    /// Flash the badge (restarts it; `false` clears a running one).
    pub badge: bool,
    /// Text of the badge, cleaned; `None` is the built-in English default.
    pub badge_text: Option<String>,
    pub reduced_motion: bool,
    pub arrow: Option<(f64, f64)>,
}

/// A parsed stdin command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Show(Show),
    Move {
        output: String,
    },
    Glide {
        frames: Vec<Pose>,
        frame_ms: f64,
    },
    Jump {
        x: f64,
        y: f64,
    },
    Press {
        x: f64,
        y: f64,
    },
    Look(Look),
    Hide,
    /// `scope` matters for `hide` only; `restore` ends any hide.
    Capture {
        phase: CapturePhase,
        id: u64,
        scope: CaptureScope,
    },
}

#[derive(Deserialize)]
struct XY {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase", rename_all_fields = "camelCase")]
enum Raw {
    Show {
        output: String,
        #[serde(default)]
        look: Look,
        #[serde(default)]
        badge: bool,
        #[serde(default)]
        badge_text: Option<String>,
        #[serde(default)]
        reduced_motion: bool,
        #[serde(default)]
        arrow: Option<XY>,
    },
    Move {
        output: String,
    },
    Glide {
        frames: Vec<Vec<f64>>,
        #[serde(default)]
        frame_ms: Option<f64>,
    },
    Jump {
        x: f64,
        y: f64,
    },
    Press {
        x: f64,
        y: f64,
    },
    Look {
        look: Look,
    },
    Hide,
    Capture {
        phase: CapturePhase,
        id: u64,
        #[serde(default)]
        scope: CaptureScope,
    },
}

fn finite(v: f64, what: &str) -> Result<f64, String> {
    if v.is_finite() && v.abs() < 1e7 {
        Ok(v)
    } else {
        Err(format!("{what} out of range"))
    }
}

/// Parse one command line (without its newline). `Err` carries the message
/// of the non-fatal error event.
pub fn parse_command(line: &str) -> Result<Command, String> {
    let raw: Raw = serde_json::from_str(line).map_err(|e| {
        let mut m = format!("bad command: {e}");
        m.truncate(200);
        m
    })?;
    Ok(match raw {
        Raw::Show {
            output,
            look,
            badge,
            badge_text,
            reduced_motion,
            arrow,
        } => {
            let arrow = match arrow {
                Some(p) => Some((finite(p.x, "arrow.x")?, finite(p.y, "arrow.y")?)),
                None => None,
            };
            Command::Show(Show {
                output,
                look,
                badge,
                badge_text: badge_text.as_deref().and_then(crate::badge::clean_text),
                reduced_motion,
                arrow,
            })
        }
        Raw::Move { output } => Command::Move { output },
        Raw::Glide { frames, frame_ms } => {
            if frames.is_empty() {
                return Err("glide: no frames".into());
            }
            if frames.len() > MAX_GLIDE_FRAMES {
                return Err(format!("glide: more than {MAX_GLIDE_FRAMES} frames"));
            }
            let frame_ms = frame_ms.unwrap_or(16.0);
            if !(1.0..=1000.0).contains(&frame_ms) {
                return Err("glide: frameMs must be 1..1000".into());
            }
            let mut out = Vec::with_capacity(frames.len());
            for f in &frames {
                if !(2..=4).contains(&f.len()) {
                    return Err("glide: a frame is [x, y, deg, lift]".into());
                }
                out.push(Pose {
                    x: finite(f[0], "glide x")?,
                    y: finite(f[1], "glide y")?,
                    deg: finite(f.get(2).copied().unwrap_or(0.0), "glide deg")?,
                    lift: finite(f.get(3).copied().unwrap_or(0.0), "glide lift")?,
                });
            }
            Command::Glide {
                frames: out,
                frame_ms,
            }
        }
        Raw::Jump { x, y } => Command::Jump {
            x: finite(x, "x")?,
            y: finite(y, "y")?,
        },
        Raw::Press { x, y } => Command::Press {
            x: finite(x, "x")?,
            y: finite(y, "y")?,
        },
        Raw::Look { look } => Command::Look(look),
        Raw::Hide => Command::Hide,
        Raw::Capture { phase, id, scope } => Command::Capture { phase, id, scope },
    })
}

/// A stdout event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "event",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    Ready {
        protocol: u32,
        backend: &'static str,
        alpha_modifier: bool,
        fractional_scale: bool,
        badge: bool,
        capture_scopes: &'static [&'static str],
        outputs: Vec<OutputInfo>,
    },
    Shown {
        output: String,
        scale: f64,
    },
    Hidden,
    /// The output layout changed (hotplug, mode, scale, position): the full
    /// list, same shape as in `ready`.
    Outputs {
        outputs: Vec<OutputInfo>,
    },
    Ack {
        id: u64,
    },
    Error {
        message: String,
        fatal: bool,
    },
}

impl Event {
    pub fn error(message: impl Into<String>, fatal: bool) -> Self {
        Event::Error {
            message: message.into(),
            fatal,
        }
    }

    /// The JSON line, newline included.
    pub fn line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"event":"error","message":"event serialization failed","fatal":false}"#.into()
        });
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(line: &str) -> Show {
        let Command::Show(s) = parse_command(line).unwrap() else {
            panic!("not a show")
        };
        s
    }

    #[test]
    fn badge_forms_old_and_new() {
        // protocol-1 callers: badge true/false only, no text
        let s = show(r#"{"cmd":"show","output":"A","badge":true}"#);
        assert!(s.badge && s.badge_text.is_none());
        let s = show(r#"{"cmd":"show","output":"A","badge":false}"#);
        assert!(!s.badge && s.badge_text.is_none());
        let s = show(r#"{"cmd":"show","output":"A"}"#);
        assert!(!s.badge && s.badge_text.is_none());
        // new: the text overrides the built-in default
        let s = show(
            r#"{"cmd":"show","output":"A","badge":true,"badgeText":"Claude nutzt deinen Computer"}"#,
        );
        assert!(s.badge);
        assert_eq!(
            s.badge_text.as_deref(),
            Some("Claude nutzt deinen Computer")
        );
        // text without badge:true is carried but draws nothing (controller)
        let s = show(r#"{"cmd":"show","output":"A","badge":false,"badgeText":"x"}"#);
        assert!(!s.badge && s.badge_text.as_deref() == Some("x"));
        // empty, blank and null fall back to the default
        for t in ["\"\"", "\"  \\n \"", "null"] {
            let s = show(&format!(
                r#"{{"cmd":"show","output":"A","badge":true,"badgeText":{t}}}"#
            ));
            assert!(s.badge_text.is_none(), "{t}");
        }
        // control characters become spaces
        let s = show(r#"{"cmd":"show","output":"A","badge":true,"badgeText":"a\nb"}"#);
        assert_eq!(s.badge_text.as_deref(), Some("a b"));
        // absurd lengths are cut
        let long = "y".repeat(10_000);
        let s = show(&format!(
            r#"{{"cmd":"show","output":"A","badge":true,"badgeText":"{long}"}}"#
        ));
        assert_eq!(
            s.badge_text.unwrap().chars().count(),
            crate::badge::MAX_TEXT_CHARS
        );
        // a wrong type is a non-fatal bad command like any other field
        assert!(
            parse_command(r#"{"cmd":"show","output":"A","badge":true,"badgeText":5}"#).is_err()
        );
    }

    #[test]
    fn parses_show() {
        let c = parse_command(
            r#"{"cmd":"show","output":"DP-1","look":"rim","badge":true,"reducedMotion":true,"arrow":{"x":10,"y":20.5}}"#,
        )
        .unwrap();
        assert_eq!(
            c,
            Command::Show(Show {
                output: "DP-1".into(),
                look: Look::Rim,
                badge: true,
                badge_text: None,
                reduced_motion: true,
                arrow: Some((10.0, 20.5)),
            })
        );
        let c = parse_command(r#"{"cmd":"show","output":"X","arrow":null}"#).unwrap();
        let Command::Show(s) = c else { panic!() };
        assert_eq!(s.look, Look::Glow);
        assert_eq!(s.arrow, None);
        assert!(!s.reduced_motion);
    }

    #[test]
    fn parses_the_rest() {
        assert_eq!(
            parse_command(r#"{"cmd":"move","output":"HDMI-A-1"}"#).unwrap(),
            Command::Move {
                output: "HDMI-A-1".into()
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"jump","x":1,"y":2}"#).unwrap(),
            Command::Jump { x: 1.0, y: 2.0 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"press","x":-5,"y":2}"#).unwrap(),
            Command::Press { x: -5.0, y: 2.0 }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"look","look":"glow"}"#).unwrap(),
            Command::Look(Look::Glow)
        );
        assert_eq!(parse_command(r#"{"cmd":"hide"}"#).unwrap(), Command::Hide);
        assert_eq!(
            parse_command(r#"{"cmd":"capture","phase":"restore","id":7}"#).unwrap(),
            Command::Capture {
                phase: CapturePhase::Restore,
                id: 7,
                scope: CaptureScope::All
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"capture","phase":"hide","id":8,"scope":"cursor"}"#).unwrap(),
            Command::Capture {
                phase: CapturePhase::Hide,
                id: 8,
                scope: CaptureScope::Cursor
            }
        );
        assert_eq!(
            parse_command(r#"{"cmd":"capture","phase":"hide","id":9,"scope":"all"}"#).unwrap(),
            Command::Capture {
                phase: CapturePhase::Hide,
                id: 9,
                scope: CaptureScope::All
            }
        );
        // unknown extra fields are ignored (forward compatible)
        assert_eq!(
            parse_command(r#"{"cmd":"hide","future":1}"#).unwrap(),
            Command::Hide
        );
    }

    #[test]
    fn parses_glide() {
        let c = parse_command(
            r#"{"cmd":"glide","frames":[[0,0,0,0],[5,6,1.5,0.2],[7,8]],"frameMs":20}"#,
        )
        .unwrap();
        let Command::Glide { frames, frame_ms } = c else {
            panic!()
        };
        assert_eq!(frame_ms, 20.0);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1].deg, 1.5);
        assert_eq!(frames[2], Pose::at(7.0, 8.0));
        let Command::Glide { frame_ms, .. } =
            parse_command(r#"{"cmd":"glide","frames":[[1,2]]}"#).unwrap()
        else {
            panic!()
        };
        assert_eq!(frame_ms, 16.0);
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "not json",
            "{}",
            r#"{"cmd":"fly"}"#,
            r#"{"cmd":"show"}"#,
            r#"{"cmd":"show","output":"X","look":"sparkle"}"#,
            r#"{"cmd":"jump","x":"1","y":2}"#,
            r#"{"cmd":"glide","frames":[]}"#,
            r#"{"cmd":"glide","frames":[[1]]}"#,
            r#"{"cmd":"glide","frames":[[1,2,3,4,5]]}"#,
            r#"{"cmd":"glide","frames":[[1,2]],"frameMs":0}"#,
            r#"{"cmd":"jump","x":1e300,"y":2}"#,
            r#"{"cmd":"capture","phase":"hide"}"#,
            r#"{"cmd":"capture","phase":"peek","id":1}"#,
            r#"{"cmd":"capture","phase":"hide","id":1,"scope":"window"}"#,
            r#"{"cmd":"capture","phase":"hide","id":1,"scope":null}"#,
            r#"{"cmd":"capture","phase":"hide","id":-1}"#,
            r#"[1,2]"#,
        ] {
            assert!(parse_command(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn serializes_events() {
        let ready = Event::Ready {
            protocol: PROTOCOL,
            backend: "layer-shell",
            alpha_modifier: true,
            fractional_scale: false,
            badge: false,
            capture_scopes: CAPTURE_SCOPES,
            outputs: vec![OutputInfo {
                name: "HEADLESS-1".into(),
                x: 0,
                y: 0,
                width: 1280,
                height: 800,
                scale: 1.0,
            }],
        };
        assert_eq!(
            ready.line(),
            "{\"event\":\"ready\",\"protocol\":1,\"backend\":\"layer-shell\",\"alphaModifier\":true,\"fractionalScale\":false,\"badge\":false,\"captureScopes\":[\"all\",\"cursor\"],\"outputs\":[{\"name\":\"HEADLESS-1\",\"x\":0,\"y\":0,\"width\":1280,\"height\":800,\"scale\":1.0}]}\n"
        );
        assert_eq!(Event::Hidden.line(), "{\"event\":\"hidden\"}\n");
        assert_eq!(
            Event::Outputs { outputs: vec![] }.line(),
            "{\"event\":\"outputs\",\"outputs\":[]}\n"
        );
        assert_eq!(
            Event::Ack { id: 3 }.line(),
            "{\"event\":\"ack\",\"id\":3}\n"
        );
        assert_eq!(
            Event::error("x", true).line(),
            "{\"event\":\"error\",\"message\":\"x\",\"fatal\":true}\n"
        );
        assert_eq!(
            Event::Shown {
                output: "A".into(),
                scale: 1.5
            }
            .line(),
            "{\"event\":\"shown\",\"output\":\"A\",\"scale\":1.5}\n"
        );
    }
}
