//! The per-host actions menu — rendering. Logic lives in `app::state::hostmenu`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

impl App {
    pub(crate) fn host_menu_rows(&self) -> Vec<ui::FocusRow> {
        self.host_menu_actions().into_iter().map(|(_, r)| r).collect()
    }

    pub(crate) fn host_menu_card_rect(
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::Fonts,
        subtitle: &str,
        rows: usize,
    ) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, rows)
    }

    pub(crate) fn render_host_menu(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let rows = self.host_menu_rows();
        let subtitle = self.host_menu_subtitle();
        let card = Self::host_menu_card_rect(screen_w, screen_h, fonts, &subtitle, rows.len());
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::render_list_modal(
            painter,
            text_cache,
            fonts,
            card,
            &self.host_menu_title(),
            &subtitle,
            &rows,
        )
    }
}
