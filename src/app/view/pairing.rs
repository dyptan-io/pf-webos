//! Pairing modal rendering. Logic lives in `app::state::pairing`.
use crate::app::App;
use crate::app::PAIRING_SUBTITLE;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

/// All y-positions on pairing card, computed once (keeps renderer, hit-test, and tile prep in sync).
pub(crate) struct PairingLayout {
    pub(crate) button: Rect,
    pub(crate) button_caption_y: i32,
    pub(crate) or_y: i32,
    pub(crate) pin_caption_y: i32,
    pub(crate) pin_y: i32,
    pub(crate) status_y: i32,
    /// The card's inner column, for full-width rules and centred captions.
    pub(crate) content: Rect,
}

/// Request access button height and card side inset.
const PAIRING_BUTTON_H: u32 = 64;
const PAIRING_MARGIN: i32 = 40;

impl App {
    pub(crate) fn pairing_layout(card: Rect, fonts: &ui::Fonts) -> PairingLayout {
        let content = Rect::new(
            card.x() + PAIRING_MARGIN,
            card.y(),
            card.width().saturating_sub(PAIRING_MARGIN as u32 * 2),
            0,
        );
        let header_end = ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, card, PAIRING_SUBTITLE);
        let button = Rect::new(content.x(), header_end + 26, content.width(), PAIRING_BUTTON_H);
        let button_caption_y = button.y() + button.height() as i32 + 12;
        let or_y = button_caption_y + fonts.raster.height(fonts.value) + 20;
        let pin_caption_y = or_y + fonts.raster.height(fonts.value) + 20;
        let pin_y = pin_caption_y + fonts.raster.height(fonts.value) + 14;
        let status_y = pin_y + ui::PAIRING_DIGIT_H as i32 + 22;
        PairingLayout {
            button,
            button_caption_y,
            or_y,
            pin_caption_y,
            pin_y,
            status_y,
            content,
        }
    }

    /// Card rect, sized from layout plus room for up-to-two-line status.
    pub(crate) fn pairing_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            let l = Self::pairing_layout(probe, fonts);
            let status_room = 2 * (fonts.raster.height(fonts.value) + 6);
            (l.status_y + status_room + 26) as u32
        })
    }

    /// Request access button rect.
    pub(crate) fn pairing_request_button_rect(card: Rect, fonts: &ui::Fonts) -> Rect {
        Self::pairing_layout(card, fonts).button
    }

    /// PIN row top y-position.
    pub(crate) fn pairing_pin_row_y(card: Rect, fonts: &ui::Fonts) -> i32 {
        Self::pairing_layout(card, fonts).pin_y
    }

    pub(crate) fn render_pairing(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = Self::pairing_card_rect(screen_w, screen_h, fonts);
        let l = Self::pairing_layout(card, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;

        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            "Pair with host",
            ui::WHITE,
            PAIRING_SUBTITLE,
            ui::MUTED,
        )?;

        // Primary first, and visually primary: approving on the host is the path that
        // always works, whereas the PIN needs the host's pairing page open and armed.
        // The shell draws it unfocused-but-filled; the focused copy is a separate
        // `Tile::ModalFocusElement` (see `prepare_tiles`).
        ui::draw_primary_button(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            l.button,
            ui::PAIRING_REQUEST_LABEL,
        )?;
        Self::draw_centred_caption(
            painter,
            text_cache,
            fonts.raster,
            fonts.value,
            l.content,
            l.button_caption_y,
            "Then approve this TV on the host.",
        )?;

        ui::draw_or_divider(painter, text_cache, fonts.raster, fonts.value, l.content, l.or_y, "or")?;

        Self::draw_centred_caption(
            painter,
            text_cache,
            fonts.raster,
            fonts.value,
            l.content,
            l.pin_caption_y,
            "Enter the PIN shown on the host.",
        )?;
        for (i, digit) in self.pin_digits.iter().enumerate() {
            let rect = ui::pairing_digit_rect(card, l.pin_y, i);
            let drawn = ui::draw_card(painter, rect, false);
            let text = digit.to_string();
            let tw = fonts.raster.measure(fonts.title, &text).0;
            ui::draw_text(
                painter,
                text_cache,
                fonts.raster,
                fonts.title,
                &text,
                drawn.x() + (drawn.width() as i32 - tw as i32) / 2,
                drawn.y() + (drawn.height() as i32 - fonts.raster.height(fonts.title)) / 2,
                ui::WHITE,
            )?;
        }

        if let Some(status) = &self.pairing_status {
            let color = if self.pairing_busy { ui::MUTED } else { ui::ERROR_RED };
            ui::draw_text_wrapped(
                painter,
                text_cache,
                fonts.raster,
                fonts.value,
                status,
                l.content.x(),
                l.status_y,
                l.content.width(),
                color,
                6,
            )?;
        }
        Ok(())
    }

    /// Centred caption line (option labels on either side of "or" rule).
    fn draw_centred_caption(
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        raster: &dyn ui::TextRaster,
        font: ui::FontId,
        content: Rect,
        y: i32,
        text: &str,
    ) -> Result<()> {
        let w = raster.measure(font, text).0 as i32;
        ui::draw_text(
            painter,
            text_cache,
            raster,
            font,
            text,
            content.x() + (content.width() as i32 - w) / 2,
            y,
            ui::MUTED,
        )?;
        Ok(())
    }
}
