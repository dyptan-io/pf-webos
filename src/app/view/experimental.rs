//! Experimental screen rendering. Logic lives in `app::state::experimental`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, FocusRow, Painter};
use anyhow::Result;

impl App {
    pub(crate) fn experimental_rows(&self) -> Vec<FocusRow> {
        ui::experimental_rows(&self.settings, crate::platform::webos::game_mode::is_rooted())
    }

    /// Row count without building the `FocusRow` vec — for card/hit-test sizing.
    pub(crate) fn experimental_row_count(&self) -> usize {
        ui::experimental_row_count(crate::platform::webos::game_mode::is_rooted())
    }

    pub(crate) fn experimental_subtitle(&self) -> String {
        "Unstable, off by default.".to_string()
    }

    /// `rows` is the live experimental-row count (`experimental_row_count`) — one shorter when
    /// the Game mode row is hidden on a non-rooted TV, so the card sizes to what's shown.
    pub(crate) fn experimental_card_rect(
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::Fonts,
        subtitle: &str,
        rows: usize,
    ) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, rows)
    }

    pub(crate) fn render_experimental(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let subtitle = self.experimental_subtitle();
        let rows = self.experimental_rows();
        let card = Self::experimental_card_rect(screen_w, screen_h, fonts, &subtitle, rows.len());
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::render_list_modal(painter, text_cache, fonts, card, "Experimental", &subtitle, &rows)
    }
}
