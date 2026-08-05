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

/// The display-PIN modal's subtitle (also used for height measurement, like `PAIRING_SUBTITLE`).
///
/// It carries the instruction too. The card had the same thing said twice in two places and two
/// alignments — a left-aligned subtitle saying the ceremony is inverted, then a centred caption
/// above the digits saying where to type them. One left-aligned paragraph in the header is the
/// shape every other modal already uses.
pub(crate) const DISPLAY_PIN_SUBTITLE: &str =
    "This host pairs the other way round. Enter this PIN on the host (Sunshine: Troubleshooting → PIN).";

/// The captions between the card's rows. Constants because the layout has to measure them: they
/// wrap to the card's inner column like every other modal's body text, so how many lines each
/// takes decides where the row below it starts.
const PAIRING_BUTTON_CAPTION: &str = "Then approve this TV on the host.";
const PAIRING_PIN_CAPTION: &str = "Enter the PIN shown on the host.";

/// Line spacing within a wrapped caption, matching the status line's.
const CAPTION_LINE_GAP: i32 = 6;

/// The display-PIN card's y-positions. A separate layout rather than a variant of
/// [`PairingLayout`]: there is no button and no "or" rule to place, and it gains a warning line
/// the entry layout has no room for.
pub(crate) struct DisplayPinLayout {
    pub(crate) pin_y: i32,
    pub(crate) status_y: i32,
    pub(crate) content: Rect,
}

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
        let or_y = button_caption_y + Self::caption_h(fonts, content.width(), PAIRING_BUTTON_CAPTION) + 20;
        let pin_caption_y = or_y + fonts.raster.height(fonts.value) + 20;
        let pin_y = pin_caption_y + Self::caption_h(fonts, content.width(), PAIRING_PIN_CAPTION) + 14;
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

    pub(crate) fn display_pin_layout(card: Rect, fonts: &ui::Fonts) -> DisplayPinLayout {
        let content = Rect::new(
            card.x() + PAIRING_MARGIN,
            card.y(),
            card.width().saturating_sub(PAIRING_MARGIN as u32 * 2),
            0,
        );
        let header_end = ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, card, DISPLAY_PIN_SUBTITLE);
        let pin_y = header_end + 26;
        let status_y = pin_y + ui::PAIRING_DIGIT_H as i32 + 22;
        DisplayPinLayout {
            pin_y,
            status_y,
            content,
        }
    }

    /// The active pairing card, sized from whichever layout is in play plus room for an
    /// up-to-two-line status. Every geometry caller goes through this, so the layout choice
    /// is made in exactly one place.
    pub(crate) fn pairing_card_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        let display_pin = self.pairing_is_display_pin();
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            let status_room = 2 * (fonts.raster.height(fonts.value) + 6);
            let status_y = if display_pin {
                Self::display_pin_layout(probe, fonts).status_y
            } else {
                Self::pairing_layout(probe, fonts).status_y
            };
            (status_y + status_room + 26) as u32
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
        let card = self.pairing_card_rect(screen_w, screen_h, fonts);
        if self.pairing_is_display_pin() {
            return self.render_display_pin(painter, text_cache, fonts, card);
        }
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
            PAIRING_BUTTON_CAPTION,
        )?;

        ui::draw_or_divider(painter, text_cache, fonts.raster, fonts.value, l.content, l.or_y, "or")?;

        Self::draw_centred_caption(
            painter,
            text_cache,
            fonts.raster,
            fonts.value,
            l.content,
            l.pin_caption_y,
            PAIRING_PIN_CAPTION,
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

    /// The `GameStream` layout: we show the PIN, the user types it into the host's web UI, and
    /// this card just waits. Nothing on it is focusable, which is why it draws its digits into the
    /// shell rather than as a `Tile::ModalFocusElement`.
    fn render_display_pin(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        card: Rect,
    ) -> Result<()> {
        let l = Self::display_pin_layout(card, fonts);
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
            DISPLAY_PIN_SUBTITLE,
            ui::MUTED,
        )?;

        // Placeholders until the PIN exists — a card with an empty row where four digits belong
        // reads as a broken modal rather than as one still starting up.
        let shown: Vec<char> = self
            .pairing_pin_shown
            .as_deref()
            .map_or_else(|| "····".chars().collect(), |pin| pin.chars().collect());
        for (i, ch) in shown.iter().enumerate().take(self.pin_digits.len()) {
            let rect = ui::pairing_digit_rect(card, l.pin_y, i);
            let drawn = ui::draw_card(painter, rect, false);
            let text = ch.to_string();
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

    /// Height one caption occupies, wrapped to `content_w` — what the row below it is placed from.
    fn caption_h(fonts: &ui::Fonts, content_w: u32, text: &str) -> i32 {
        let lines = ui::wrap_text(fonts.raster, fonts.value, text, content_w).len().max(1) as i32;
        lines * fonts.raster.height(fonts.value) + (lines - 1) * CAPTION_LINE_GAP
    }

    /// Centred caption (the option labels either side of the "or" rule, and the display-PIN
    /// instruction). Wrapped to the card's inner column like every other modal's body text —
    /// unwrapped, the longest of them ran past the card edge on a narrower card. Each line is
    /// centred individually, so the block stays symmetric.
    fn draw_centred_caption(
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        raster: &dyn ui::TextRaster,
        font: ui::FontId,
        content: Rect,
        y: i32,
        text: &str,
    ) -> Result<()> {
        let mut cursor_y = y;
        for line in ui::wrap_text(raster, font, text, content.width()) {
            let w = raster.measure(font, &line).0 as i32;
            ui::draw_text(
                painter,
                text_cache,
                raster,
                font,
                &line,
                content.x() + (content.width() as i32 - w) / 2,
                cursor_y,
                ui::MUTED,
            )?;
            cursor_y += raster.height(font) + CAPTION_LINE_GAP;
        }
        Ok(())
    }
}
