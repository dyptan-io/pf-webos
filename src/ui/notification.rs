//! Reusable transient toast notifications for the streaming overlay: a short message that
//! appears for a fixed hold then fades out. Styled like the stats overlay panel (same glass
//! background/radius, see [`render_notification_tile`]); the fade reuses the app-wide
//! [`anim_frac`] easing so it matches the modals' curve. One slot — a new [`Notification::show`]
//! replaces whatever is on screen.

use super::{anim_frac, draw_text, Painter, TextCache, OVERLAY_FADE, WHITE};
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::ttf::Font;
use std::time::{Duration, Instant};

/// Fully opaque for this long before the fade begins.
const HOLD: Duration = Duration::from_secs(2);

/// A single-slot transient toast. The message is owned so callers don't have to keep it
/// alive across ticks; [`Notification::frame`] drives both the fade alpha and expiry.
#[derive(Default)]
pub struct Notification {
    active: Option<(String, Instant)>,
}

impl Notification {
    pub fn new() -> Self {
        Self { active: None }
    }

    /// Show `text` from now: full opacity for [`HOLD`], then an [`OVERLAY_FADE`] fade.
    pub fn show(&mut self, text: impl Into<String>) {
        self.active = Some((text.into(), Instant::now()));
    }

    /// `(text, alpha)` to draw this tick, or `None` once fully faded. Clears the slot on
    /// expiry, so a return flipping to `None` marks the on→off edge for the caller.
    pub fn frame(&mut self) -> Option<(String, f32)> {
        let shown = self.active.as_ref()?.1;
        let elapsed = shown.elapsed();
        if elapsed >= HOLD + OVERLAY_FADE {
            self.active = None;
            return None;
        }
        let alpha = if elapsed < HOLD {
            1.0
        } else {
            (1.0 - anim_frac(Some(shown + HOLD), OVERLAY_FADE)).clamp(0.0, 1.0)
        };
        Some((self.active.as_ref()?.0.clone(), alpha))
    }
}

/// Single-line notification panel, styled like the stats overlay's glass background.
pub fn render_notification_tile(font: &Font, text: &str) -> Result<Painter> {
    let pad = 18i32;
    let (tw, _) = font.size_of(text).unwrap_or((0, 0));
    let w = tw + 2 * pad as u32;
    let h = (font.height() + 2 * pad) as u32;
    let mut p = Painter::new(w.max(1), h.max(1));
    let mut tc = TextCache::new();
    p.fill_rounded_rect(Rect::new(0, 0, w, h), 14, Color::RGBA(0x14, 0x10, 0x1f, 0x90));
    draw_text(&mut p, &mut tc, font, text, pad, pad, WHITE)?;
    Ok(p)
}
