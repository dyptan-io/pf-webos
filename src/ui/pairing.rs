//! The pairing modal's PIN digit boxes and request-access button.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use crate::ui::render::Rect;
use crate::ui::text_raster::{FontId, TextRaster};
use anyhow::Result;

/// PIN digit box size/gap — shared by `pairing_digit_rect` and the digit
/// tiles so they can never disagree.
pub const PAIRING_DIGIT_W: u32 = 64;
pub const PAIRING_DIGIT_H: u32 = 80;
pub const PAIRING_DIGIT_GAP: i32 = 14;

/// PIN digit `index`'s rect within `card`, given the row's top `y` (from
/// `modal_header_end_y` plus a fixed gap) — the one place this layout formula
/// lives, shared by `render_pairing` and `app.rs`'s `draw_list`.
pub fn pairing_digit_rect(card: Rect, digit_y: i32, index: usize) -> Rect {
    let total_w = 4 * PAIRING_DIGIT_W as i32 + 3 * PAIRING_DIGIT_GAP;
    let start_x = card.x() + (card.width() as i32 - total_w) / 2;
    Rect::new(
        start_x + index as i32 * (PAIRING_DIGIT_W as i32 + PAIRING_DIGIT_GAP),
        digit_y,
        PAIRING_DIGIT_W,
        PAIRING_DIGIT_H,
    )
}

pub const PAIRING_REQUEST_LABEL: &str = "Request access";

/// A focused card tile with centered text — a padded transparent tile holding
/// one `draw_card(.., false)` box (no CPU inflate; the zoom is a GPU animation
/// in `app.rs`'s `draw_list`) with `text` centered in it. Backs both pairing
/// focus tiles below.
pub fn render_card_text_tile(
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    font: FontId,
    text: &str,
    w: u32,
    h: u32,
) -> Result<Painter> {
    let pad = ROW_TILE_PAD;
    let mut p = Painter::new(w + 2 * pad as u32, h + 2 * pad as u32);
    let drawn = draw_card(&mut p, Rect::new(pad, pad, w, h), false);
    let tw = raster.measure(font, text).0;
    draw_text(
        &mut p,
        text_cache,
        raster,
        font,
        text,
        drawn.x() + (drawn.width() as i32 - tw as i32) / 2,
        drawn.y() + (drawn.height() as i32 - raster.height(font)) / 2,
        WHITE,
    )?;
    Ok(p)
}

/// One PIN digit, focused, as its own zoom-animated tile — composited by the
/// GPU over the shell's unfocused digit boxes, same pattern as
/// `render_focus_row_tile`.
pub fn render_pairing_digit_tile(
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    font_title: FontId,
    digit: u8,
) -> Result<Painter> {
    render_card_text_tile(
        text_cache,
        raster,
        font_title,
        &digit.to_string(),
        PAIRING_DIGIT_W,
        PAIRING_DIGIT_H,
    )
}

/// The "Request access" button, focused, as its own zoom-animated tile — accent-filled
/// like the shell's copy (see `ui::draw_primary_button`), not the surface-card treatment
/// the digit tiles use, so the primary action keeps its emphasis while focused.
pub fn render_pairing_button_tile(
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    font_label: FontId,
    w: u32,
    h: u32,
) -> Result<Painter> {
    let pad = ROW_TILE_PAD;
    let mut p = Painter::new(w + 2 * pad as u32, h + 2 * pad as u32);
    draw_primary_button(
        &mut p,
        text_cache,
        raster,
        font_label,
        Rect::new(pad, pad, w, h),
        PAIRING_REQUEST_LABEL,
    )?;
    Ok(p)
}
