use crate::ui::render::{Rect, RectF};
use std::time::{Duration, Instant};

/// Durations for focus-pop and launch-fade animations.
pub const FOCUS_POP: Duration = Duration::from_millis(140);
pub const LAUNCH_FADE: Duration = Duration::from_millis(600);

/// How long the connecting screen's hero pan takes to cross the whole image. Set
/// well past any plausible handshake so a real load only ever shows a slow drift.
pub const HERO_PAN: Duration = Duration::from_secs(75);

/// The hero's fade, in and back out to black again. Slower than `LAUNCH_FADE`: the card zoom is
/// a reaction to a button press and has to feel immediate, while this is a scene settling in —
/// and leaving it settles out on the same curve it arrived on.
pub const HERO_FADE: Duration = Duration::from_millis(1_300);

/// How long the hero stays up once it appears, [`HERO_FADE`] included. A floor, not a cap: a
/// slower handshake keeps it up longer. Long enough to read as a loading screen with a visible
/// pan rather than a flash. Only ever paid by a game that has wide art — with no hero to hold,
/// the hand-off is unchanged.
pub const HERO_MIN_SHOW: Duration = Duration::from_millis(2_700);

/// Longest a launch waits for one thing that may never arrive, spent on a screen the user is
/// watching. Two of those, on the same budget: the decoder reporting that it presents
/// (`runtime::stream`'s reveal, for the launches with no hero to hold), and a hero still being
/// fetched off the host (`app::hero`, before it gives up and hands over without one).
pub const STREAM_REVEAL_WAIT: Duration = Duration::from_millis(1_500);

/// Longest the loading screen runs before handing over regardless of the connect thread.
/// Only a backstop — `session::connect` has its own timeouts.
pub const HERO_LOADING_MAX: Duration = Duration::from_secs(30);

/// How much the hero is darkened once fully faded in, so it reads as a backdrop
/// rather than as content.
pub const HERO_SCRIM_ALPHA: f32 = 70.0;

/// Destination for the connecting screen's slow left-to-right pan: the hero scaled to
/// full screen height (so it is wider than the screen, by design — see the art loader's
/// `HERO_ASPECT`) and slid leftwards across that slack, off the edges of the target.
///
/// Subpixel on purpose. At this speed the image travels well under a pixel per frame, so
/// a whole-pixel destination would hold still for ten-odd frames and then jump; the
/// fractional offset plus bilinear filtering makes it a continuous drift instead.
///
/// Linear rather than eased — a constant drift reads as deliberate motion, while an
/// ease would visibly stall on a loading screen of unpredictable length.
pub fn hero_pan_dst(img_w: u32, img_h: u32, screen_w: u32, screen_h: u32, elapsed: Duration) -> RectF {
    let full = RectF {
        x: 0.0,
        y: 0.0,
        w: screen_w as f32,
        h: screen_h as f32,
    };
    if img_h == 0 || img_w == 0 {
        return full;
    }
    let scaled_w = img_w as f32 * (screen_h as f32 / img_h as f32);
    let slack = (scaled_w - screen_w as f32).max(0.0);
    let f = (elapsed.as_secs_f32() / HERO_PAN.as_secs_f32()).clamp(0.0, 1.0);
    RectF {
        x: -slack * f,
        w: scaled_w,
        ..full
    }
}

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
