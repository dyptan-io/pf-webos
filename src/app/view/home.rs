//! Home screen grid pixel geometry. Navigation/selection logic lives in
//! `app::state::home`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self};

impl App {
    /// Extra vertical offset for grid index `idx`'s row — `ui::PINNED_SECTION_GAP`
    /// once, for every row from the "rest" section on, `0` for a row still inside
    /// the pinned front block (see `pinned_rows`).
    fn extra_row_gap(&self, idx: usize, columns: usize) -> i32 {
        let pinned_rows = self.pinned_rows(columns);
        if pinned_rows > 0 && idx / columns.max(1) >= pinned_rows {
            ui::PINNED_SECTION_GAP
        } else {
            0
        }
    }

    /// `grid_card_rect`, translated by `extra_row_gap` — everything except the
    /// current scroll offset; `scrolled_card_rect` applies that on top.
    pub(crate) fn unscrolled_card_rect(&self, idx: usize, columns: usize, grid_x: i32, available_w: u32) -> Rect {
        let r = ui::grid_card_rect(idx, columns, grid_x, available_w);
        let extra = self.extra_row_gap(idx, columns);
        Rect::new(r.x(), r.y() + extra, r.width(), r.height())
    }

    /// `unscrolled_card_rect`, translated by the current scroll offset — every
    /// draw-list card position starts from this.
    pub(crate) fn scrolled_card_rect(&self, idx: usize, columns: usize, grid_x: i32, available_w: u32) -> Rect {
        let r = self.unscrolled_card_rect(idx, columns, grid_x, available_w);
        Rect::new(r.x(), r.y() - self.grid_scroll, r.width(), r.height())
    }

    /// The divider between the pinned front block and the rest, centered in the
    /// gap `extra_row_gap` adds there, scrolled like any other grid content.
    pub(crate) fn pinned_separator_rect(&self, columns: usize, grid_x: i32, available_w: u32) -> Option<Rect> {
        if !self.has_pinned_divider(columns) {
            return None;
        }
        let rows = self.pinned_rows(columns);
        let (_, card_h) = ui::grid_card_size(available_w, columns);
        let y = ui::GRID_TOP_Y + rows as i32 * (card_h as i32 + ui::GRID_GAP) - ui::GRID_GAP / 2
            + ui::PINNED_SECTION_GAP / 2
            - self.grid_scroll;
        Some(Rect::new(
            grid_x + ui::GRID_PAD,
            y,
            available_w.saturating_sub(2 * ui::GRID_PAD as u32),
            1,
        ))
    }
}
