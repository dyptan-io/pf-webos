use crate::ui::render::Rect;
use std::time::{Duration, Instant};

/// Durations for focus-pop and launch-fade animations.
pub const FOCUS_POP: Duration = Duration::from_millis(140);
pub const LAUNCH_FADE: Duration = Duration::from_millis(600);

/// Cubic ease-out function.
pub fn ease(f: f32) -> f32 {
    1.0 - (1.0 - f).powi(3)
}

/// Eased progress 0..=1 of animation; 1.0 when done/absent.
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

/// Scale up from (1.0 - shrink) to full size. "Pop in" counterpart to `zoom_rect`.
pub fn pop_in_rect(base: Rect, frac: f32, shrink: f32) -> Rect {
    if frac >= 1.0 {
        base
    } else {
        zoom_rect(base, 1.0 - frac, -shrink)
    }
}
