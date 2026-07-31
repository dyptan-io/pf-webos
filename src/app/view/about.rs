//! About screen rendering. Logic lives in `app::state::about`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

impl App {
    pub(crate) fn about_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::about_card_rect(screen_w, screen_h)
    }

    /// The shell only — header and card chrome. The document body is its own
    /// `Tile::ScrollContent(Screen::About)` tile, composited separately (see the
    /// module docs), so this no longer depends on scroll position at all.
    pub(crate) fn render_about(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = ui::about_card_rect(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            "About & licenses",
            ui::WHITE,
            &format!("Version {}", ui::VERSION),
            ui::MUTED,
        )?;
        Ok(())
    }
}
