//! Shared scroll bookkeeping for modal content lists (uniform-stride rows or
//! wrapped text lines). Offset clamping, scroll-into-view, and fade logic
//! extracted so any modal can reuse it. Caller-agnostic to rendering/pixels.
use std::time::Instant;

/// Scroll offset bookkeeping. `total`/`visible` passed per-call (not stored)
/// to avoid stale copies disagreeing with caller's geometry.
#[derive(Clone, Copy)]
pub struct ScrollWindow {
    pub offset: usize,
    /// When `offset` last changed (scrollbar shows then fades).
    pub shown_at: Option<Instant>,
}

impl ScrollWindow {
    pub fn new() -> Self {
        Self {
            offset: 0,
            shown_at: None,
        }
    }

    /// Clamped offset (use where it feeds layout formulas, not raw field).
    pub fn clamped(&self, total: usize, visible: usize) -> usize {
        self.offset.min(total.saturating_sub(visible))
    }

    /// Scroll to keep `focused` visible (no wraparound). Returns whether moved.
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
