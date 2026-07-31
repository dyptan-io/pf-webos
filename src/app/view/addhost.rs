//! Add-host / edit-host address form rendering. Logic lives in `app::state::addhost`
//! (and `app::state::edithost`).
use crate::app::App;
use crate::core::screen::Screen;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

const ADD_HOST_SUBTITLE: &str = "Enter the host's IP address.";

impl App {
    /// Screen-specific subtitle to avoid layout overflow.
    pub(crate) fn address_subtitle(&self) -> String {
        match self.screen {
            Screen::EditHost => {
                let name = self
                    .edit_host_index
                    .and_then(|i| self.entries.get(i))
                    .map_or_else(String::new, |e| e.name().to_string());
                format!("New IP address for {name}. Its pairing is kept.")
            }
            _ => ADD_HOST_SUBTITLE.to_string(),
        }
    }

    /// For `SDL_SetTextInputRect` (webOS OSK ignores it).
    pub(crate) fn address_field_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        let subtitle = self.address_subtitle();
        let card = self.address_card_rect(screen_w, screen_h, fonts);
        let after_subtitle_y = ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, card, &subtitle);
        Rect::new(
            card.x() + 32,
            after_subtitle_y + 20,
            card.width().saturating_sub(64),
            80,
        )
    }

    /// Lifts clear of OSK via `keyboard_modal_card`.
    pub(crate) fn address_card_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        let subtitle = self.address_subtitle();
        self.keyboard_modal_card(screen_w, screen_h, |probe| {
            let header_end = ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, probe, &subtitle);
            (header_end + 20 + 80 + 32) as u32 // field + bottom margin
        })
    }

    pub(crate) fn render_add_host(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        self.render_host_address_form(painter, text_cache, fonts, screen_w, screen_h, "Add host")
    }

    /// Shared by `AddHost` and `EditHost`.
    pub(crate) fn render_host_address_form(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
        title: &str,
    ) -> Result<()> {
        let subtitle = self.address_subtitle();
        let card = self.address_card_rect(screen_w, screen_h, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;

        let after_subtitle_y = ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            title,
            ui::WHITE,
            &subtitle,
            ui::MUTED,
        )?;

        let field = Rect::new(
            card.x() + 32,
            after_subtitle_y + 20,
            card.width().saturating_sub(64),
            80,
        );
        let drawn = ui::draw_card(painter, field, true);
        let text_x = drawn.x() + 24;
        let typed = self.add_host.display_text();
        let text_w = fonts.raster.measure(fonts.title, &typed).0;
        ui::draw_text(
            painter,
            text_cache,
            fonts.raster,
            fonts.title,
            &typed,
            text_x,
            drawn.y() + (drawn.height() as i32 - fonts.raster.height(fonts.title)) / 2,
            ui::WHITE,
        )?;
        // A blinkless text-cursor bar right after what's typed so far — there's
        // no fixed-width mask anymore to show *where* editing happens, so this
        // stands in for it.
        let caret = Rect::new(
            text_x + text_w as i32 + 6,
            drawn.y() + 16,
            3,
            drawn.height().saturating_sub(32),
        );
        painter.fill_rect(caret, ui::ACCENT_BRIGHT);
        Ok(())
    }
}
