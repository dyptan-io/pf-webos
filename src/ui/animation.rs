//! Shared animation clocks and the GPU zoom-pop rect math.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use std::time::{Duration, Instant};
use sdl2::rect::Rect;

// Shared by every GPU-scale zoom-pop in the app — the grid's card focus-pop
// (`app.rs`), every pre-stream modal's focused-widget tile, and the in-stream
// disconnect dialog's (`main.rs`) — so there's exactly one implementation of
// "ease a clock, then scale a rect around its center" instead of one per caller.

/// How long a focus-pop (zoom-in) animation runs — the grid's card, every
/// pre-stream modal's focused widget, and the disconnect dialog's button.
pub const FOCUS_POP: Duration = Duration::from_millis(140);

/// Cubic ease-out for the animation fractions below.
pub fn ease(f: f32) -> f32 {
    1.0 - (1.0 - f).powi(3)
}

/// Eased 0..=1 progress of an animation started at `t`; 1.0 when done/absent.
pub fn anim_frac(anim: Option<Instant>, dur: Duration) -> f32 {
    match anim {
        Some(t) => ease((t.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0)),
        None => 1.0,
    }
}

/// Scales `base` by `1.0 + growth * frac` around its own center — the GPU
/// zoom-in technique behind every focus-pop in the app. The source tile is
/// rasterized once, at its literal size; only this destination rect changes
/// per frame, so the zoom costs nothing beyond a GPU texture copy at a
/// different size.
pub fn zoom_rect(base: Rect, frac: f32, growth: f32) -> Rect {
    let scale = 1.0 + growth * frac;
    let cx = base.x() as f32 + base.width() as f32 / 2.0;
    let cy = base.y() as f32 + base.height() as f32 / 2.0;
    let tw = base.width() as f32 * scale;
    let th = base.height() as f32 * scale;
    Rect::new((cx - tw / 2.0) as i32, (cy - th / 2.0) as i32, tw as u32, th as u32)
}

