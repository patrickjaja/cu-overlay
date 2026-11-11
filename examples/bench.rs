//! Time the per-frame drawing work of a glide and a press:
//! `cargo run --release --example bench -- [scale] [lift]`.

use std::time::Instant;

use cu_overlay::anim::press_frame;
use cu_overlay::geom::to_px;
use cu_overlay::render::{self, ArrowParams, CURSOR_HOT, CURSOR_SIZE, RING_HALF, RING_SIZE};

fn main() {
    let scale: f32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.5);
    // optional fixed lift (default: sweep 0..1 like a glide)
    let fixed_lift: Option<f32> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    let s120 = (scale * 120.0).round() as u32;
    let n = to_px(CURSOR_SIZE, s120) as usize;
    let mut b = vec![0u8; n * n * 4];
    let iters = 300;
    let t = Instant::now();
    for i in 0..iters {
        let p = ArrowParams {
            hot_x: CURSOR_HOT as f32 + (i % 32) as f32 / 32.0,
            hot_y: CURSOR_HOT as f32,
            deg: (i as f32 / 10.0).sin() * 4.0,
            lift: fixed_lift.unwrap_or((i % 60) as f32 / 60.0),
            press_scale: 1.0,
        };
        render::draw_arrow(&mut b, n, n, scale, &p, 1.0);
    }
    println!(
        "arrow {n}x{n}: {:.3} ms/frame",
        t.elapsed().as_secs_f64() * 1000.0 / iters as f64
    );
    let n = to_px(RING_SIZE, s120) as usize;
    let mut b = vec![0u8; n * n * 4];
    let t = Instant::now();
    for i in 0..iters {
        let f = press_frame((i * 3) as f64, false).expect("frame");
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
    }
    println!(
        "ring {n}x{n}: {:.3} ms/frame",
        t.elapsed().as_secs_f64() * 1000.0 / iters as f64
    );
}
