//! Shared scroll bookkeeping for modal content lists (uniform-stride rows or
//! wrapped text lines) — extracted from the Settings modal's original
//! hand-written offset-clamp/scroll-into-view logic so any modal with
//! overflowing content (About's document, a future long `ListModal`) can
//! share the same offset clamping, "scroll into view", and scroll-indicator
//! fade-in bookkeeping instead of re-deriving it. Knows nothing about
//! rendering, tile identity, or pixels — callers already have their own pure
//! `total`/`visible` formulas (a row count vs. a wrapped-line count use
//! different stride math) and pass them in.
use std::time::Instant;

/// Offset bookkeeping for one scrollable list of uniform-stride units (rows
/// or wrapped text lines). `total`/`visible` are passed to each call rather
/// than stored, since callers already compute them from screen geometry —
/// storing a second, possibly-stale copy here would just invite the two to
/// disagree.
pub struct ScrollWindow {
    pub offset: usize,
    /// When `offset` last changed — a modal's scrollbar shows briefly after
    /// this, then fades (see `SCROLL_INDICATOR_HOLD`/`_FADE` in `app::mod`).
    pub shown_at: Option<Instant>,
}

impl ScrollWindow {
    pub fn new() -> Self {
        Self {
            offset: 0,
            shown_at: None,
        }
    }

    /// `offset` is only updated by `scroll_into_view`/`scroll_by`/`page`, so it
    /// can be stale after a resize (a rotated screen, a font-size change) —
    /// use this instead of the raw field wherever it feeds a layout formula.
    pub fn clamped(&self, total: usize, visible: usize) -> usize {
        self.offset.min(total.saturating_sub(visible))
    }

    /// Moves `offset` just enough to keep `focused` inside the visible
    /// window (no wraparound — wrapping a scrolled list would silently jump
    /// the scroll position across the whole thing). Returns whether it moved.
    pub fn scroll_into_view(&mut self, focused: usize, total: usize, visible: usize) -> bool {
        let mut offset = self.clamped(total, visible);
        if focused < offset {
            offset = focused;
        } else if focused >= offset + visible {
            offset = focused + 1 - visible;
        }
        self.set(offset)
    }

    /// Wheel/line-step scroll by `delta` units (+/-), clamped to the valid
    /// range. Returns whether `offset` moved.
    pub fn scroll_by(&mut self, delta: i64, total: usize, visible: usize) -> bool {
        let before = self.clamped(total, visible) as i64;
        let max_offset = total.saturating_sub(visible) as i64;
        let next = (before + delta).clamp(0, max_offset) as usize;
        self.set(next)
    }

    /// Pages by `page_units` (About's Left/Right paging), clamped the same way.
    pub fn page(&mut self, page_units: usize, forward: bool, total: usize, visible: usize) -> bool {
        let step = page_units.max(1) as i64;
        self.scroll_by(if forward { step } else { -step }, total, visible)
    }

    fn set(&mut self, offset: usize) -> bool {
        let moved = offset != self.offset;
        self.offset = offset;
        if moved {
            self.shown_at = Some(Instant::now());
        }
        moved
    }
}

/// Tracks which contiguous slice `[start, start+len)` of a long, uniform-stride
/// list is currently baked into a content tile, for lists too tall to fit one
/// GPU texture whole (About's ~12k wrapped lines). A modal whose whole content
/// always fits under `budget` (Settings' 9 rows, `HostMenu`'s handful of rows)
/// never sees `recenter_if_needed` return more than once — this degenerates to
/// "bake everything, once" for them, same as before this type existed.
pub struct ContentWindow {
    pub start: usize,
    pub len: usize,
}

impl ContentWindow {
    pub fn new() -> Self {
        Self { start: 0, len: 0 }
    }

    /// Returns `Some(new_start)` if the window needs (re)baking to keep
    /// `offset` (plus `visible` units after it) within `margin` units of an
    /// edge — `None` if the currently baked window still covers it. The new
    /// window is up to `budget` units, recentered around `offset`.
    pub fn recenter_if_needed(
        &self,
        offset: usize,
        visible: usize,
        total: usize,
        budget: usize,
        margin: usize,
    ) -> Option<usize> {
        if total <= budget {
            return if self.start != 0 || self.len != total {
                Some(0)
            } else {
                None
            };
        }
        let end = self.start + self.len;
        let near_start = self.start > 0 && offset < self.start + margin;
        let near_end = end < total && offset + visible + margin > end;
        if self.len == 0 || near_start || near_end {
            let half = budget.saturating_sub(visible) / 2;
            let max_start = total - budget;
            Some(offset.saturating_sub(half).min(max_start))
        } else {
            None
        }
    }
}
