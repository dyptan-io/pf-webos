//! Diagnostics screen rendering. Logic lives in `app::state::diagnostics`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, FocusRow, Painter};
use anyhow::Result;

impl App {
    pub(crate) fn diagnostics_rows(&self) -> Vec<FocusRow> {
        ui::diagnostics_rows(&self.settings)
    }

    pub(crate) fn diagnostics_subtitle(&self) -> String {
        "Debug aids for on-device investigation.".to_string()
    }

    pub(crate) fn diagnostics_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, subtitle: &str) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, ui::DIAGNOSTICS_ROW_COUNT)
    }

    pub(crate) fn render_diagnostics(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let subtitle = self.diagnostics_subtitle();
        let card = Self::diagnostics_card_rect(screen_w, screen_h, fonts, &subtitle);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::render_list_modal(
            painter,
            text_cache,
            fonts,
            card,
            "Diagnostics",
            &subtitle,
            &self.diagnostics_rows(),
        )
    }
}
