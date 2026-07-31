//! Experimental screen rendering. Logic lives in `app::state::experimental`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, FocusRow, Painter};
use anyhow::Result;

impl App {
    pub(crate) fn experimental_rows(&self) -> Vec<FocusRow> {
        ui::experimental_rows(&self.settings)
    }

    pub(crate) fn experimental_subtitle(&self) -> String {
        "Unstable, off by default. Frame pacer also toggles live with the Blue button.".to_string()
    }

    pub(crate) fn experimental_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, subtitle: &str) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, ui::EXPERIMENTAL_ROW_COUNT)
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
        let card = Self::experimental_card_rect(screen_w, screen_h, fonts, &subtitle);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::render_list_modal(
            painter,
            text_cache,
            fonts,
            card,
            "Experimental",
            &subtitle,
            &self.experimental_rows(),
        )
    }
}
