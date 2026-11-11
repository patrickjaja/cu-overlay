//! Render the renderer core's pieces to raw BGRA files for inspection:
//! `cargo run --example snapshot -- <dir> [scale]` writes `arrow.bgra`
//! (CURSOR_SIZE square), `ring.bgra` (RING_SIZE square, mid-press) and
//! `glow.bgra` (640x400), each with a `.txt` holding `width height`.

use cu_overlay::anim::press_frame;
use cu_overlay::geom::to_px;
use cu_overlay::render::{self, ArrowParams, CURSOR_HOT, CURSOR_SIZE, RING_HALF, RING_SIZE};

fn save(dir: &str, name: &str, buf: &[u8], w: usize, h: usize) {
    std::fs::write(format!("{dir}/{name}.bgra"), buf).expect("write");
    std::fs::write(format!("{dir}/{name}.txt"), format!("{w} {h}\n")).expect("write");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| ".".into());
    let scale: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(2.0);
    let s120 = (scale * 120.0).round() as u32;

    let n = to_px(CURSOR_SIZE, s120) as usize;
    let mut b = vec![0u8; n * n * 4];
    for (name, lift, deg, press) in [
        ("arrow", 0.0, 0.0, 1.0),
        ("arrow_lift", 1.0, 8.0, 1.0),
        ("arrow_press", 0.0, 0.0, 0.87),
    ] {
        let p = ArrowParams {
            hot_x: CURSOR_HOT as f32,
            hot_y: CURSOR_HOT as f32,
            deg,
            lift,
            press_scale: press,
        };
        render::draw_arrow(&mut b, n, n, scale, &p, 1.0);
        save(&dir, name, &b, n, n);
    }

    let n = to_px(RING_SIZE, s120) as usize;
    let mut b = vec![0u8; n * n * 4];
    let f = press_frame(250.0, false).expect("mid-press");
    render::draw_ring(
        &mut b,
        n,
        n,
        scale,
        RING_HALF as f32,
        RING_HALF as f32,
        &f,
        1.0,
    );
    save(&dir, "ring", &b, n, n);

    let (w, h) = (to_px(640, s120) as usize, to_px(400, s120) as usize);
    let mut b = vec![0u8; w * h * 4];
    render::draw_glow(&mut b, w, h, scale, &render::GLOW_PEAK);
    save(&dir, "glow", &b, w, h);
}
