//! Shared open/close fade bookkeeping for modal-like overlays — the pre-stream `App`'s
//! `Screen` modals and the in-stream disconnect dialog both use one of these so every
//! dialog in the app opens/closes on the same clock and curve instead of each re-deriving
//! it (see `docs/NOTES.md` for why that drifted apart once: the disconnect dialog and
//! stats overlay live in `main.rs`'s streaming loop, which has no `App`/`Screen` to hook).

use super::anim_frac;
use std::time::{Duration, Instant};

/// Shared fade-in/out duration for transient in-stream overlays (toast notifications,
/// the stats overlay, the log-tail overlay) — one curve so toggling any of them feels
/// the same.
pub const OVERLAY_FADE: Duration = Duration::from_millis(400);

/// `T` is whatever the caller needs preserved while closing (e.g. which screen was
/// open) so the fade-out can keep rendering it after the live state has already moved
/// on — use `()` if there's only ever one thing this could be.
pub struct ModalFade<T = ()> {
    open_since: Option<Instant>,
    closing: Option<(Instant, T)>,
}

impl<T: Copy + PartialEq> Default for ModalFade<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + PartialEq> ModalFade<T> {
    pub fn new() -> Self {
        Self {
            open_since: None,
            closing: None,
        }
    }

    /// Starts (or restarts) the open fade. Leaves an in-flight close alone.
    pub fn open(&mut self) {
        self.open_since = Some(Instant::now());
    }

    /// `open`, but cancels any in-flight close (for single-overlay callers).
    pub fn reopen(&mut self) {
        self.open();
        self.closing = None;
    }

    /// Starts the close fade, carrying `payload` for `closing_frame` to hand back.
    pub fn close(&mut self, payload: T) {
        self.closing = Some((Instant::now(), payload));
    }

    /// Cancels an in-flight close only if it's fading out `payload`.
    pub fn cancel_closing(&mut self, payload: T) {
        if self.closing.is_some_and(|(_, p)| p == payload) {
            self.closing = None;
        }
    }

    /// Returns `(alpha, payload)` while a close is in flight; `None` otherwise.
    pub fn closing_frame(&self, dur: Duration) -> Option<(f32, T)> {
        let (t, payload) = self.closing.filter(|(t, _)| t.elapsed() < dur)?;
        Some((1.0 - anim_frac(Some(t), dur), payload))
    }

    /// Open-fade alpha: eases 0.0 -> 1.0, `1.0` once finished or if never opened.
    pub fn open_alpha(&self, dur: Duration) -> f32 {
        anim_frac(self.open_since, dur)
    }

    /// Alpha for a simple show/hide overlay driven by `shown`: `Some` through the close
    /// fade even after `shown` has already flipped to `false` (so the last frame doesn't
    /// cut instantly), and `None` once fully faded and hidden.
    pub fn visibility_alpha(&self, dur: Duration, shown: bool) -> Option<f32> {
        if let Some((alpha, _)) = self.closing_frame(dur) {
            return Some(alpha);
        }
        shown.then(|| self.open_alpha(dur))
    }

    /// Whether an open or close fade is still mid-flight (i.e. hasn't yet reached its
    /// steady state). Pure and non-mutating, unlike `tick` — safe to call just to pick a
    /// redraw cadence.
    pub fn is_animating(&self, dur: Duration) -> bool {
        self.open_since.is_some_and(|t| t.elapsed() < dur) || self.closing.is_some_and(|(t, _)| t.elapsed() < dur)
    }

    /// Advances the clock; returns whether either fade is still in flight.
    pub fn tick(&mut self, dur: Duration) -> bool {
        let mut animating = false;
        if let Some(t) = self.open_since {
            if t.elapsed() >= dur {
                self.open_since = None;
            }
            animating = true;
        }
        if let Some((t, _)) = self.closing {
            if t.elapsed() >= dur {
                self.closing = None;
            }
            animating = true;
        }
        animating
    }
}
