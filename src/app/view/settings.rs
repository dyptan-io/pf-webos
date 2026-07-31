//! The settings modal's rendering: row list layout, dropdown overlay geometry, shell.
//! Logic lives in `app::state::settings`.
use crate::app::App;
use crate::app::DROPDOWN_FADE;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

impl App {
    /// How many settings rows are *fully* visible. Capped at the live row count so a hidden
    /// row (Color range on NDL) leaves no empty slot.
    ///
    /// When the list overflows, one row's worth of budget is spent on `SETTINGS_PEEK` instead
    /// — the partially-visible sliver the bottom fade dissolves. Computed without the peek
    /// first, because a list that fits entirely has nothing below to peek at and should not
    /// give up the space.
    pub(crate) fn settings_visible_rows(&self, screen_h: u32) -> usize {
        let stride = ui::settings_row_stride();
        let total = ui::settings_row_count(&self.settings);
        let budget =
            screen_h.saturating_sub(ui::SETTINGS_CHROME_TOP + self.settings_chrome_bottom() + ui::SETTINGS_EDGE_MARGIN);
        if (budget / stride) as usize >= total {
            return total.max(1);
        }
        // Both peeks come out of the budget, not just the bottom one — see `SETTINGS_PEEK`.
        ((budget.saturating_sub(2 * ui::SETTINGS_PEEK) / stride) as usize).clamp(1, total)
    }

    /// Card space below the list: minimal, unless the high-bitrate caution line needs room.
    pub(crate) fn settings_chrome_bottom(&self) -> u32 {
        if self.settings.bitrate_kbps > ui::BITRATE_WARN_KBPS {
            ui::SETTINGS_WARN_CHROME
        } else {
            ui::SETTINGS_CHROME_BOTTOM
        }
    }

    /// Height of the scrolling viewport: the fully-visible rows plus a peek strip past each
    /// edge while the list overflows. Deliberately *not* a whole multiple of the row stride
    /// when scrolling — see [`ui::SETTINGS_PEEK`].
    pub(crate) fn settings_content_h(&self, screen_h: u32) -> u32 {
        let visible = self.settings_visible_rows(screen_h);
        let peeks = if visible < ui::settings_row_count(&self.settings) {
            2 * ui::SETTINGS_PEEK
        } else {
            0
        };
        visible as u32 * ui::settings_row_stride() + peeks
    }

    /// Scrolls `settings_focused` into view; updates scroll indicator.
    pub(crate) fn scroll_settings_into_view(&mut self, screen_h: u32) {
        let visible = self.settings_visible_rows(screen_h);
        self.scroll
            .scroll_into_view(self.settings_focused, ui::settings_row_count(&self.settings), visible);
    }

    /// Settings card and content rects (shared by render and hit-test).
    pub(crate) fn settings_layout(&self, screen_w: u32, screen_h: u32) -> (Rect, Rect) {
        let content_h = self.settings_content_h(screen_h);
        let card_h = content_h + ui::SETTINGS_CHROME_TOP + self.settings_chrome_bottom();
        // Widened from 0.56 to fit the scroll indicator on the right edge.
        let card = ui::modal_card_rect(screen_w, screen_h, 0.62, card_h);
        let content = Rect::new(
            card.x() + 40,
            card.y() + ui::SETTINGS_CHROME_TOP as i32,
            card.width().saturating_sub(80),
            content_h,
        );
        (card, content)
    }
    pub(crate) fn render_settings(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let (card, content) = self.settings_layout(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::draw_text(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            "Settings",
            card.x() + 40,
            card.y() + 36,
            ui::WHITE,
        )?;
        painter.fill_rect(
            Rect::new(card.x() + 40, card.y() + 88, card.width().saturating_sub(80), 1),
            crate::ui::render::Color::RGBA(0xff, 0xff, 0xff, 0x1e),
        );

        // The row list itself is drawn separately — see `Tile::ScrollContent` — so
        // scrolling never re-rasterizes this shell; only a value/dropdown change does.
        // The open dropdown's panel is drawn separately too — see `Tile::DropdownOverlay`
        // — so it composites *after* `Tile::ScrollContent` instead of being covered by it.

        if self.settings.bitrate_kbps > ui::BITRATE_WARN_KBPS {
            ui::draw_text(
                painter,
                text_cache,
                fonts.raster,
                fonts.value,
                "May be unstable on Wi-Fi — try Ethernet if streaming drops.",
                content.x(),
                content.y() + content.height() as i32 + 16,
                ui::WARNING,
            )?;
        }
        Ok(())
    }

    /// Where a dropdown opened from settings row `row` anchors its option
    /// overlay — one row below it. Shared by `render_settings` and `draw_list`,
    /// which both need it (as a whole, or per-option via
    /// `ui::dropdown_option_rect`).
    /// Positioned from a pixel scroll offset rather than a viewport-local row index, since a
    /// gliding list puts its rows at continuous offsets. `scroll_px` of 0 is the unscrolled
    /// case (Diagnostics).
    pub(crate) fn dropdown_overlay_rect_at_px(content: Rect, row: usize, scroll_px: i32) -> Rect {
        let y = ui::focus_row_rect_at_px(content, row + 1, scroll_px).y();
        Rect::new(content.x(), y, content.width(), 0)
    }

    /// `(row, focused, alpha)` for the open dropdown or its close-fade; `None` if neither.
    pub(crate) fn dropdown_draw_state(&self) -> Option<(usize, usize, f32)> {
        if let Some(dd) = &self.dropdown {
            Some((dd.row, dd.focused, self.dropdown_fade.open_alpha(DROPDOWN_FADE)))
        } else {
            self.dropdown_fade
                .closing_frame(DROPDOWN_FADE)
                .map(|(alpha, (row, focused))| (row, focused, alpha))
        }
    }
}
