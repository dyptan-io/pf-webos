use super::*;
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::ttf::Font;

/// A centered glass card of `(width_frac * screen_w, height)`.
/// Centered glass card.
pub fn modal_card_rect(screen_w: u32, screen_h: u32, width_frac: f32, height: u32) -> Rect {
    let w = (screen_w as f32 * width_frac).round() as u32;
    let x = (screen_w as i32 - w as i32) / 2;
    let y = (screen_h as i32 - height as i32) / 2;
    Rect::new(x, y, w, height)
}

/// How much of the panel webOS's on-screen keyboard occupies, measured on-device: it
/// takes roughly the bottom half and its height barely varies between layouts.
///
/// This has to be a constant because SDL gives us no way to ask. `SDL_webOS.h` publishes
/// only cursor / panel-resolution / refresh-rate / exported-window calls,
/// `SDL_IsScreenKeyboardShown` is a bare bool, and the fork's internal `input_panel_rect`
/// isn't reachable from the public API. `SDL_SetTextInputRect` (which this app does set,
/// to the field) is the correct contract for "keep this region clear", but the webOS OSK
/// ignores it.
pub const KEYBOARD_PANEL_FRAC: f32 = 0.5;

/// Smallest gap left above a card that has been lifted clear of the keyboard, so a tall
/// one can't be pushed off the top of the panel.
const KEYBOARD_MIN_TOP: i32 = 24;

/// A modal card centred in whatever space the on-screen keyboard leaves.
///
/// With the keyboard down this is just [`modal_card_rect`] — the card sits where every
/// other modal sits. With it up, the card is centred in the band above the panel rather
/// than pinned to the very top: the point is to clear the keyboard, not to jam the card
/// against the screen edge.
pub fn modal_card_rect_above_keyboard(
    screen_w: u32,
    screen_h: u32,
    width_frac: f32,
    height: u32,
    keyboard_shown: bool,
) -> Rect {
    if !keyboard_shown {
        return modal_card_rect(screen_w, screen_h, width_frac, height);
    }
    let w = (screen_w as f32 * width_frac).round() as u32;
    let x = (screen_w as i32 - w as i32) / 2;
    let available = (screen_h as f32 * (1.0 - KEYBOARD_PANEL_FRAC)).round() as i32;
    let y = ((available - height as i32) / 2).max(KEYBOARD_MIN_TOP);
    Rect::new(x, y, w, height)
}

/// Draw the modal card surface.
pub fn draw_modal_card(painter: &mut Painter, rect: Rect) {
    draw_card_shadow(painter, rect, MODAL_RADIUS);
    painter.fill_rounded_rect(rect, MODAL_RADIUS, SIDEBAR_BG);
    painter.stroke_rounded_rect(rect, MODAL_RADIUS, Color::RGBA(0xff, 0xff, 0xff, 0x18), 1.5);
}

/// Width fraction shared by the confirm-style modals (forget host, send logs, stop
/// streaming, quit app) — narrower than the scrollable `ListModal` screens.
pub const SIMPLE_MODAL_WIDTH_FRAC: f32 = 0.40;

/// A centered [`SIMPLE_MODAL_WIDTH_FRAC`]-wide card whose *height* is derived from its
/// own content: `content_height` receives a zero-y/height probe card at the final width
/// and returns the card's total height. Shared by every confirm modal so they size
/// identically whether they render through `App` (forget/send-logs) or `main.rs`'s
/// in-stream/quit dialog.
pub fn simple_modal_card(screen_w: u32, screen_h: u32, content_height: impl FnOnce(Rect) -> u32) -> Rect {
    let w = (screen_w as f32 * SIMPLE_MODAL_WIDTH_FRAC).round() as u32;
    let height = content_height(Rect::new(0, 0, w, 0));
    modal_card_rect(screen_w, screen_h, SIMPLE_MODAL_WIDTH_FRAC, height)
}

pub fn modal_close_rect(card_rect: Rect) -> Rect {
    const SIZE: u32 = 44;
    const MARGIN: i32 = 20;
    Rect::new(
        card_rect.x() + card_rect.width() as i32 - MARGIN - SIZE as i32,
        card_rect.y() + MARGIN,
        SIZE,
        SIZE,
    )
}

/// A horizontal rule broken by a centred word — the "or" between two mutually exclusive
/// choices.
///
/// The pairing card offers two independent ways to pair, and a sentence saying so is not
/// enough: without a visual break the two blocks read as *steps*, i.e. "fill in the PIN,
/// then press the button". The rule makes the exclusivity structural rather than something
/// the user has to read and remember.
/// Horizontal rule with centered word (e.g., "or" between two exclusive options).
pub fn draw_or_divider(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font: &Font,
    content: Rect,
    y: i32,
    word: &str,
) -> Result<()> {
    let word_w = font.size_of(word).map_or(0, |(w, _)| w) as i32;
    let gap = 18i32;
    let line_y = y + font.height() / 2;
    let rule = Color::RGBA(0xff, 0xff, 0xff, 0x1e);
    let half = (content.width() as i32 - word_w - 2 * gap) / 2;
    if half > 0 {
        painter.fill_rect(Rect::new(content.x(), line_y, half as u32, 1), rule);
        painter.fill_rect(
            Rect::new(content.x() + content.width() as i32 - half, line_y, half as u32, 1),
            rule,
        );
    }
    draw_text(
        painter,
        text_cache,
        font,
        word,
        content.x() + (content.width() as i32 - word_w) / 2,
        y,
        MUTED,
    )?;
    Ok(())
}

pub fn draw_primary_button(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font: &Font,
    rect: Rect,
    label: &str,
) -> Result<()> {
    draw_card_shadow(painter, rect, CARD_RADIUS);
    painter.fill_rounded_rect(rect, CARD_RADIUS, ACCENT);
    let tw = font.size_of(label).map_or(0, |(w, _)| w) as i32;
    draw_text(
        painter,
        text_cache,
        font,
        label,
        rect.x() + (rect.width() as i32 - tw) / 2,
        rect.y() + (rect.height() as i32 - font.height()) / 2,
        WHITE,
    )?;
    Ok(())
}
