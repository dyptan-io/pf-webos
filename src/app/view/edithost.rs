//! Editing a saved host's address — rendering (reuses the add-host form). Logic lives in
//! `app::state::edithost`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

impl App {
    pub(crate) fn edit_host_card_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        self.address_card_rect(screen_w, screen_h, fonts)
    }

    pub(crate) fn render_edit_host(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        self.render_host_address_form(painter, text_cache, fonts, screen_w, screen_h, "Edit address")
    }
}
