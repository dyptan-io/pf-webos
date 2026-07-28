//! Shared open/close fade bookkeeping for modal-like overlays — the pre-stream `App`'s
//! `Screen` modals and the in-stream disconnect dialog both use one of these so every
//! dialog in the app opens/closes on the same clock and curve instead of each re-deriving
//! it (see `docs/NOTES.md` for why that drifted apart once: the disconnect dialog and
//! stats overlay live in `main.rs`'s streaming loop, which has no `App`/`Screen` to hook).

use super::anim_frac;
use std::time::{Duration, Instant};

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

    /// Starts (or restarts) the open fade. Leaves an in-flight close alone — call
    /// `cancel_closing` too if this open should pre-empt it (see its docs).
    pub fn open(&mut self) {
        self.open_since = Some(Instant::now());
    }

    /// `open`, but unconditionally cancels any in-flight close — for callers with a
    /// single overlay instance, where a fresh open always means the same thing as
    /// whatever was closing (the disconnect dialog; `App` uses `cancel_closing`
    /// instead, since a different `Screen` closing behind a new one must keep fading).
    pub fn reopen(&mut self) {
        self.open();
        self.closing = None;
    }

    /// Starts the close fade, carrying `payload` for `closing_frame` to hand back.
    pub fn close(&mut self, payload: T) {
        self.closing = Some((Instant::now(), payload));
    }

    /// Cancels an in-flight close only if it's fading out `payload` — call from an
    /// open site so reopening the very thing that was still closing doesn't leave a
    /// stale close-fade fighting the new open (see `ModalFade`'s docs on `T`).
    pub fn cancel_closing(&mut self, payload: T) {
        if self.closing.is_some_and(|(_, p)| p == payload) {
            self.closing = None;
        }
    }

    /// `(alpha 0..1, payload)` while a close is in flight — `None` once it's run its
    /// course (or none was started), meaning the caller should fall back to whatever
    /// live state it actually has.
    pub fn closing_frame(&self, dur: Duration) -> Option<(f32, T)> {
        let (t, payload) = self.closing.filter(|(t, _)| t.elapsed() < dur)?;
        Some((1.0 - anim_frac(Some(t), dur), payload))
    }

    /// Open-fade alpha: eases 0.0 -> 1.0, `1.0` once finished or if never opened.
    pub fn open_alpha(&self, dur: Duration) -> f32 {
        anim_frac(self.open_since, dur)
    }

    /// Advances the clock; reports whether either fade is still in flight. The tick an
    /// animation expires on still reports `true`, so its final (fully open/closed)
    /// frame gets drawn once before this goes quiet.
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
