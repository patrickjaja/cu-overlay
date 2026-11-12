//! The X11 backend against a recording [`Wire`]: window bookkeeping,
//! opacity changes, buffer reuse, layout changes and the capture-ack
//! ordering, without an X server.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::layout::{RawLayout, RawMonitor, XRect};
use super::*;
use crate::controller::{ArrowDraw, BadgeDraw, PressDraw, Scene};
use crate::protocol::Pose;

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Create(u32, XRect),
    Configure(u32, XRect),
    Opacity(u32, u32),
    Destroy(u32),
    Alloc(u32),
    Free(u32),
    Put(u32, u32),
    Sync(u64),
    Flush,
}

/// A fake server: requests are recorded; on `flush` it "processes" them,
/// queueing a completion per put and the marker per sync, in order.
#[derive(Default)]
struct Mock {
    ops: Vec<Op>,
    raw: RawLayout,
    next: u32,
    bufs: HashMap<u32, Vec<u8>>,
    sent: usize,
    queue: VecDeque<XEv>,
    /// Hold puts' completions back (a busy server).
    hold_completions: bool,
    layout_reads: u32,
}

impl Mock {
    fn id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }
}

impl Wire for Mock {
    fn fd(&self) -> std::os::fd::RawFd {
        -1
    }
    fn flush(&mut self) -> Result<(), String> {
        for op in &self.ops[self.sent..] {
            match op {
                Op::Put(_, b) if !self.hold_completions => self.queue.push_back(XEv::ShmDone(*b)),
                Op::Sync(t) => self.queue.push_back(XEv::Synced(*t)),
                _ => {}
            }
        }
        self.ops.push(Op::Flush);
        self.sent = self.ops.len();
        Ok(())
    }
    fn poll(&mut self) -> Result<Option<XEv>, String> {
        Ok(self.queue.pop_front())
    }
    fn layout(&mut self) -> Result<RawLayout, String> {
        self.layout_reads += 1;
        Ok(self.raw.clone())
    }
    fn create_window(&mut self, r: XRect) -> Result<u32, String> {
        let id = self.id();
        self.ops.push(Op::Create(id, r));
        Ok(id)
    }
    fn configure(&mut self, win: u32, r: XRect) -> Result<(), String> {
        self.ops.push(Op::Configure(win, r));
        Ok(())
    }
    fn set_opacity(&mut self, win: u32, value: u32) -> Result<(), String> {
        self.ops.push(Op::Opacity(win, value));
        Ok(())
    }
    fn destroy_window(&mut self, win: u32) -> Result<(), String> {
        self.ops.push(Op::Destroy(win));
        Ok(())
    }
    fn alloc(&mut self, w: i32, h: i32) -> Result<u32, String> {
        let id = 1000 + self.id();
        self.bufs.insert(id, vec![0; (w * h * 4) as usize]);
        self.ops.push(Op::Alloc(id));
        Ok(id)
    }
    fn pixels(&mut self, buf: u32) -> &mut [u8] {
        self.bufs
            .get_mut(&buf)
            .map_or(&mut [], |v| v.as_mut_slice())
    }
    fn free(&mut self, buf: u32) {
        self.bufs.remove(&buf);
        self.ops.push(Op::Free(buf));
    }
    fn put(&mut self, win: u32, buf: u32, _w: i32, _h: i32) -> Result<(), String> {
        self.ops.push(Op::Put(win, buf));
        Ok(())
    }
    fn sync(&mut self, token: u64) -> Result<(), String> {
        self.ops.push(Op::Sync(token));
        Ok(())
    }
}

fn raw(s: i32) -> RawLayout {
    RawLayout {
        monitors: vec![
            RawMonitor {
                name: "Virtual-1".into(),
                rect: XRect {
                    x: 0,
                    y: 0,
                    w: 1280 * s,
                    h: 800 * s,
                },
            },
            RawMonitor {
                name: "Virtual-2".into(),
                rect: XRect {
                    x: 1280 * s,
                    y: 0,
                    w: 1280 * s,
                    h: 800 * s,
                },
            },
        ],
        root: (2560 * s, 800 * s),
        desktop: Some((2560 * s, 800 * s)),
        window_scaling: Some(s),
        xft_dpi: Some(96.0 * s as f64),
        workareas: workareas(s),
    }
}

/// Mutter's work areas: top bar and dock on the first monitor.
fn workareas(s: i32) -> Vec<XRect> {
    vec![
        XRect {
            x: 66 * s,
            y: 29 * s,
            w: 1214 * s,
            h: 771 * s,
        },
        XRect {
            x: 1280 * s,
            y: 0,
            w: 1280 * s,
            h: 800 * s,
        },
    ]
}

fn backend(s: i32) -> X11<Mock> {
    let mock = Mock {
        raw: raw(s),
        ..Mock::default()
    };
    let mut be = X11::with_wire(mock, Options::default()).unwrap();
    be.settle = Duration::from_millis(40);
    be.layout_settle = Duration::ZERO;
    be
}

fn rim(arrow: Option<(f64, f64)>, opacity: f64) -> Scene {
    Scene {
        look: Look::Rim,
        opacity,
        pulse: None,
        arrow: arrow.map(|(x, y)| ArrowDraw {
            pose: Pose::at(x, y),
            scale: 1.0,
        }),
        press: None,
        badge: None,
        animating: false,
    }
}

/// One loop turn: prepare (flush) then dispatch.
fn turn(be: &mut X11<Mock>) -> Vec<BackendEvent> {
    be.prepare().unwrap();
    be.dispatch(true).unwrap()
}

fn ops_since(be: &X11<Mock>, n: usize) -> Vec<Op> {
    be.w.ops[n..].to_vec()
}

fn windows(be: &X11<Mock>) -> [u32; 4] {
    let s = be.surf.as_ref().unwrap();
    [s.main.id, s.badge.id, s.ring.id, s.cur.id]
}

#[test]
fn ready_reports_logical_outputs_and_caps() {
    let be = backend(2);
    let o = be.outputs();
    assert_eq!(o.len(), 2);
    assert_eq!(
        (
            o[1].name.as_str(),
            o[1].x,
            o[1].width,
            o[1].height,
            o[1].scale
        ),
        ("Virtual-2", 1280, 1280, 800, 2.0)
    );
    let c = be.caps();
    assert_eq!(c.name, "x11");
    assert!(c.alpha_modifier && c.badge && !c.fractional_scale);
    assert_eq!(c.pulse_interval, None, "static glow by default");
    let mut be = backend(1);
    be.pulse = Some(Duration::from_millis(100));
    assert_eq!(be.caps().pulse_interval, Some(Duration::from_millis(100)));
    be.pulse = Some(Duration::ZERO);
    assert_eq!(be.caps().pulse_interval, None, "0 ms: static glow");
}

#[test]
fn create_makes_four_windows_bottom_to_top_and_destroy_frees_all() {
    let mut be = backend(2);
    assert!(be.create("nope").is_err());
    be.create("Virtual-2").unwrap();
    assert_eq!(turn(&mut be), vec![BackendEvent::Configured]);
    let main = XRect {
        x: 2560,
        y: 0,
        w: 2560,
        h: 1600,
    };
    let creates: Vec<XRect> =
        be.w.ops
            .iter()
            .filter_map(|o| match o {
                Op::Create(_, r) => Some(*r),
                _ => None,
            })
            .collect();
    // one logical pixel (a 1x1 X window was never shown under native scaling)
    let small = XRect {
        x: 2560,
        y: 0,
        w: 2,
        h: 2,
    };
    assert_eq!(creates, vec![main, small, small, small]);
    assert_eq!(be.current(), Some(("Virtual-2".into(), 2.0)));
    be.present(&rim(Some((1500.0, 300.0)), 1.0));
    let [m, _, ring, cur] = windows(&be);
    let ops = be.w.ops.clone();
    // rim drawn once at the output's X size, arrow window placed at
    // (L - hot) * s inside the output, both made visible
    assert!(ops.contains(&Op::Opacity(m, u32::MAX)));
    assert!(ops.contains(&Op::Opacity(cur, u32::MAX)));
    assert!(
        !ops.iter()
            .any(|o| matches!(o, Op::Opacity(w, _) if *w == ring))
    );
    let local = (1500.0 - 1280.0, 300.0);
    let hot = CURSOR_HOT as f64;
    assert!(ops.contains(&Op::Configure(
        cur,
        XRect {
            x: 2560 + ((local.0 - hot) * 2.0) as i32,
            y: ((local.1 - hot) * 2.0) as i32,
            w: CURSOR_SIZE * 2,
            h: CURSOR_SIZE * 2,
        }
    )));
    // the arrow window gets its size before the image is put into it
    let cfg = ops
        .iter()
        .position(|o| matches!(o, Op::Configure(w, _) if *w == cur))
        .unwrap();
    let put = ops
        .iter()
        .position(|o| matches!(o, Op::Put(w, _) if *w == cur))
        .unwrap();
    assert!(cfg < put);
    let n = be.w.ops.len();
    be.destroy();
    let ops = ops_since(&be, n);
    for w in windows_of(&ops) {
        assert!(ops.contains(&Op::Destroy(w)));
    }
    assert_eq!(
        ops.iter().filter(|o| matches!(o, Op::Destroy(_))).count(),
        4
    );
    assert!(ops.iter().any(|o| matches!(o, Op::Free(_))));
    assert!(be.w.bufs.is_empty(), "every buffer freed");
    assert!(!be.ready_to_draw());
}

fn windows_of(ops: &[Op]) -> Vec<u32> {
    ops.iter()
        .filter_map(|o| match o {
            Op::Destroy(w) => Some(*w),
            _ => None,
        })
        .collect()
}

#[test]
fn a_static_scene_sends_nothing_and_a_moving_arrow_repaints_only_its_window() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    be.present(&rim(Some((100.0, 100.0)), 1.0));
    turn(&mut be);
    let n = be.w.ops.len();
    be.present(&rim(Some((100.0, 100.0)), 1.0));
    turn(&mut be);
    assert_eq!(ops_since(&be, n), vec![Op::Flush], "nothing changed");
    let [m, _, _, cur] = windows(&be);
    // a whole-pixel move only moves the arrow's window
    let n = be.w.ops.len();
    be.present(&rim(Some((140.0, 120.0)), 1.0));
    let ops = ops_since(&be, n);
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], Op::Configure(w, _) if w == cur));
    // a sub-pixel move repaints the arrow's window, never the output's
    let n = be.w.ops.len();
    be.present(&rim(Some((140.5, 120.25)), 1.0));
    let ops = ops_since(&be, n);
    assert!(ops.iter().any(|o| matches!(o, Op::Put(w, _) if *w == cur)));
    assert!(!ops.iter().any(|o| matches!(o, Op::Put(w, _) if *w == m)));
    assert!(!ops.iter().any(|o| matches!(o, Op::Opacity(..))));
}

#[test]
fn capture_hide_all_acks_after_flush_round_trip_and_settle() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    be.present(&rim(Some((100.0, 100.0)), 1.0));
    turn(&mut be);
    let [m, b, ring, cur] = windows(&be);
    // the loop's Action::Ack: present the hidden scene, then sync
    let n = be.w.ops.len();
    be.present(&rim(None, 0.0));
    be.sync(7);
    let ops = ops_since(&be, n);
    let sync = ops.iter().position(|o| *o == Op::Sync(7)).unwrap();
    let hide_m = ops.iter().position(|o| *o == Op::Opacity(m, 0)).unwrap();
    let hide_c = ops.iter().position(|o| *o == Op::Opacity(cur, 0)).unwrap();
    assert!(hide_m < sync && hide_c < sync, "opacity before the marker");
    // badge and ring were never visible: nothing to send for them
    assert!(!ops.contains(&Op::Opacity(b, 0)) && !ops.contains(&Op::Opacity(ring, 0)));
    // not flushed yet: no ack
    assert!(be.dispatch(false).unwrap().is_empty());
    // flushed and the marker is back: still held for the settle time
    let t0 = Instant::now();
    assert!(turn(&mut be).is_empty());
    let wake = be.next_wakeup().expect("a settle deadline");
    assert!(wake >= t0 + Duration::from_millis(30));
    std::thread::sleep(Duration::from_millis(45));
    assert_eq!(turn(&mut be), vec![BackendEvent::Synced(7)]);
    assert_eq!(be.next_wakeup(), None);

    // restore: nothing became invisible, so the ack needs only the round trip
    be.present(&rim(Some((100.0, 100.0)), 1.0));
    be.sync(8);
    assert_eq!(turn(&mut be), vec![BackendEvent::Synced(8)]);
}

#[test]
fn capture_hide_cursor_scope_touches_only_the_cursor_windows() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    let mut s = rim(Some((300.0, 300.0)), 1.0);
    s.press = Some(PressDraw {
        x: 300.0,
        y: 300.0,
        frame: crate::anim::press_frame(100.0, false).unwrap(),
    });
    be.present(&s);
    turn(&mut be);
    let [m, b, ring, cur] = windows(&be);
    let n = be.w.ops.len();
    // the controller's cursor scope: arrow and ring gone, opacity kept
    be.present(&rim(None, 1.0));
    be.sync(3);
    let ops = ops_since(&be, n);
    assert_eq!(
        ops,
        vec![Op::Opacity(ring, 0), Op::Opacity(cur, 0), Op::Sync(3)]
    );
    assert!(
        !ops.iter()
            .any(|o| matches!(o, Op::Opacity(w, _) if *w == m || *w == b))
    );
    assert!(turn(&mut be).is_empty(), "held for the settle time");
    std::thread::sleep(Duration::from_millis(45));
    assert_eq!(turn(&mut be), vec![BackendEvent::Synced(3)]);
}

#[test]
fn destroy_counts_as_hiding_for_the_hidden_event() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    be.present(&rim(None, 1.0));
    turn(&mut be);
    be.destroy();
    be.sync(5);
    assert!(turn(&mut be).is_empty());
    std::thread::sleep(Duration::from_millis(45));
    assert_eq!(turn(&mut be), vec![BackendEvent::Synced(5)]);
}

#[test]
fn a_busy_buffer_is_not_reused_until_the_server_read_it() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    be.w.hold_completions = true;
    be.present(&rim(Some((100.0, 100.0)), 1.0));
    turn(&mut be);
    let cur = windows(&be)[3];
    let puts = |be: &X11<Mock>| -> Vec<u32> {
        be.w.ops
            .iter()
            .filter_map(|o| match o {
                Op::Put(w, b) if *w == cur => Some(*b),
                _ => None,
            })
            .collect()
    };
    be.present(&rim(Some((100.5, 100.0)), 1.0));
    turn(&mut be);
    let p = puts(&be);
    assert_eq!(p.len(), 2);
    assert_ne!(p[0], p[1], "second frame got a second buffer");
    // completions arrive: the first buffer is free again and reused
    be.w.queue.push_back(XEv::ShmDone(p[0]));
    be.w.queue.push_back(XEv::ShmDone(p[1]));
    turn(&mut be);
    be.present(&rim(Some((101.0, 100.0)), 1.0));
    let p = puts(&be);
    assert!(p[2] == p[0] || p[2] == p[1]);
    let allocs =
        be.w.ops
            .iter()
            .filter(|o| matches!(o, Op::Alloc(_)))
            .count();
    assert_eq!(allocs, 3, "main + two arrow buffers");
}

#[test]
fn expose_reputs_the_current_buffer() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    be.present(&rim(None, 1.0));
    turn(&mut be);
    let m = windows(&be)[0];
    let last =
        be.w.ops
            .iter()
            .rev()
            .find_map(|o| match o {
                Op::Put(w, b) if *w == m => Some(*b),
                _ => None,
            })
            .unwrap();
    let n = be.w.ops.len();
    be.w.queue.push_back(XEv::Expose(m));
    turn(&mut be);
    assert_eq!(ops_since(&be, n), vec![Op::Put(m, last), Op::Flush]);
}

#[test]
fn layout_changes_report_outputs_and_follow_the_shown_output() {
    let mut be = backend(1);
    be.create("Virtual-2").unwrap();
    turn(&mut be);
    be.present(&rim(None, 1.0));
    turn(&mut be);
    let m = windows(&be)[0];
    // live switch to native scaling: XSETTINGS 2, RandR stale at 1
    be.w.raw.window_scaling = Some(2);
    be.w.raw.desktop = Some((5120, 1600));
    be.w.raw.workareas = workareas(2);
    be.w.queue.push_back(XEv::Layout);
    let ev = turn(&mut be);
    assert_eq!(
        ev,
        vec![BackendEvent::OutputsChanged, BackendEvent::Changed]
    );
    assert_eq!(be.outputs()[1].scale, 2.0);
    assert_eq!(be.outputs()[1].x, 1280, "logical layout unchanged");
    let n = be.w.ops.len();
    be.present(&rim(None, 1.0));
    let ops = ops_since(&be, n);
    let r = XRect {
        x: 2560,
        y: 0,
        w: 2560,
        h: 1600,
    };
    assert!(ops.contains(&Op::Configure(m, r)));
    assert!(
        ops.iter().any(|o| matches!(o, Op::Put(w, _) if *w == m)),
        "redrawn at 2"
    );
    // an identical re-read changes nothing
    be.w.queue.push_back(XEv::Layout);
    assert!(turn(&mut be).is_empty());
    // the shown output is unplugged
    be.w.raw.monitors.pop();
    be.w.raw.workareas.pop();
    be.w.queue.push_back(XEv::Layout);
    let ev = turn(&mut be);
    assert_eq!(ev[0], BackendEvent::OutputsChanged);
    assert!(matches!(&ev[1], BackendEvent::Closed(_)));
    assert!(!be.ready_to_draw());
    assert_eq!(be.outputs().len(), 1);
}

#[test]
fn animation_frames_are_paced_by_round_trips() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    let mut s = rim(Some((100.0, 100.0)), 1.0);
    s.animating = true;
    be.present(&s);
    // a second present before the frame came back does not stack markers
    be.present(&s);
    let markers =
        be.w.ops
            .iter()
            .filter(|o| matches!(o, Op::Sync(t) if t & INTERNAL != 0))
            .count();
    assert_eq!(markers, 1);
    let t0 = Instant::now();
    let ev = turn(&mut be);
    assert!(ev.is_empty(), "marker back but the interval has not passed");
    let wake = be.next_wakeup().unwrap();
    assert!(wake > t0 && wake <= t0 + FRAME_INTERVAL);
    std::thread::sleep(FRAME_INTERVAL);
    assert_eq!(turn(&mut be), vec![BackendEvent::Frame]);
    // nothing animating: no marker, no frame
    be.present(&rim(Some((100.0, 100.0)), 1.0));
    assert!(turn(&mut be).is_empty());
    assert_eq!(be.next_wakeup(), None);
}

#[test]
fn badge_and_glow_opacity_and_off_output_arrow() {
    let mut be = backend(1);
    be.create("Virtual-1").unwrap();
    turn(&mut be);
    let s = Scene {
        look: Look::Glow,
        opacity: 1.0,
        pulse: Some(0.7),
        arrow: None,
        press: None,
        badge: Some(BadgeDraw {
            ms: 2000.0,
            text: "Claude is using your computer".into(),
        }),
        animating: false,
    };
    be.present(&s);
    let [m, b, _, cur] = windows(&be);
    assert!(be.w.ops.contains(&Op::Opacity(m, mult(0.7))));
    assert!(be.w.ops.contains(&Op::Opacity(b, u32::MAX)));
    assert!(
        be.w.ops
            .iter()
            .any(|o| matches!(o, Op::Put(w, _) if *w == b))
    );
    // the pulse changes only the main window's opacity, no redraw
    let n = be.w.ops.len();
    be.present(&Scene {
        pulse: Some(0.9),
        ..s.clone()
    });
    assert_eq!(ops_since(&be, n), vec![Op::Opacity(m, mult(0.9))]);
    // rim with an arrow on the other output: the arrow window stays invisible
    let n = be.w.ops.len();
    be.present(&rim(Some((1500.0, 100.0)), 1.0));
    let ops = ops_since(&be, n);
    assert!(
        !ops.iter()
            .any(|o| matches!(o, Op::Opacity(w, v) if *w == cur && *v > 0))
    );
    assert!(ops.contains(&Op::Opacity(b, 0)), "badge gone");
}

#[test]
fn layout_changes_are_read_once_after_a_burst() {
    let mut be = backend(1);
    be.layout_settle = Duration::from_millis(30);
    let reads = be.w.layout_reads;
    // Mutter moves _NET_DESKTOP_GEOMETRY first, XSETTINGS follows later
    be.w.raw.desktop = Some((5120, 1600));
    be.w.raw.workareas = workareas(2);
    be.w.queue.push_back(XEv::Layout);
    assert!(turn(&mut be).is_empty(), "not read yet");
    assert!(be.next_wakeup().is_some());
    std::thread::sleep(Duration::from_millis(15));
    be.w.raw.window_scaling = Some(2);
    be.w.queue.push_back(XEv::Layout);
    assert!(
        turn(&mut be).is_empty(),
        "the second change restarts the wait"
    );
    std::thread::sleep(Duration::from_millis(35));
    assert_eq!(turn(&mut be), vec![BackendEvent::OutputsChanged]);
    assert_eq!(be.w.layout_reads, reads + 1, "one read for the burst");
    // never the transient (factor 1 with the desktop at twice the root)
    let o = be.outputs();
    assert_eq!((o[1].x, o[1].width, o[1].scale), (1280, 1280, 2.0));
}
