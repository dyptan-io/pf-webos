//! The "you can only pin N games" alert — rendering. Logic lives in `app::state::pinlimit`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

/// The single OK button's fixed size.
const PIN_LIMIT_BUTTON_W: u32 = 200;
const PIN_LIMIT_BUTTON_H: u32 = 72;

impl App {
    pub(crate) fn pin_limit_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            let header_end =
                ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, probe, Self::PIN_LIMIT_MESSAGE);
            (header_end + 32 + 72 + 32) as u32
        })
    }

    pub(crate) fn render_pin_limit(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = Self::pin_limit_card_rect(screen_w, screen_h, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            "Pin limit reached",
            ui::WHITE,
            Self::PIN_LIMIT_MESSAGE,
            ui::MUTED,
        )?;
        let after_subtitle_y =
            ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, card, Self::PIN_LIMIT_MESSAGE);
        // Single centered button, always focused (no ModalFocusKey tracking).
        let button = Rect::new(
            card.x() + (card.width() as i32 - PIN_LIMIT_BUTTON_W as i32) / 2,
            after_subtitle_y + 32,
            PIN_LIMIT_BUTTON_W,
            PIN_LIMIT_BUTTON_H,
        );
        ui::draw_confirm_button(
            painter,
            text_cache,
            fonts,
            &ui::ConfirmButton {
                icon: None,
                label: "OK",
                color: ui::WHITE,
            },
            true,
            button,
        )
    }
}
