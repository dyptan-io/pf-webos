use anyhow::Result;
use sdl2::rect::Rect;

use super::*;

/// Wider than the confirm-style modals ([`SIMPLE_MODAL_WIDTH_FRAC`]) — these
/// hold full rows with icons and hint text, not a sentence and two buttons.
pub const LIST_MODAL_WIDTH_FRAC: f32 = 0.46;
/// Gap between the header's last line and the first row.
const HEADER_GAP: i32 = 24;
/// Space left below the last row, inside the card.
const BOTTOM_PAD: i32 = 24;
/// Left/right inset of the row list within the card.
const SIDE_PAD: i32 = 32;

/// The card rect for a list modal with `row_count` rows and this `subtitle` (whose
/// wrapped height moves everything below it). Mirrors `App::simple_modal_card`'s
/// probe trick: measure against a zero-height card at the final width, then place it.
pub fn list_modal_card_rect(screen_w: u32, screen_h: u32, fonts: &Fonts, subtitle: &str, row_count: usize) -> Rect {
    let w = (screen_w as f32 * LIST_MODAL_WIDTH_FRAC).round() as u32;
    let probe = Rect::new(0, 0, w, 0);
    let header_end = modal_header_end_y(fonts.label, fonts.value, probe, subtitle);
    let rows_h = row_count as i32 * (SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP);
    let height = (header_end + HEADER_GAP + rows_h + BOTTOM_PAD).max(0) as u32;
    modal_card_rect(screen_w, screen_h, LIST_MODAL_WIDTH_FRAC, height)
}

/// Where the row list starts inside `card` — the rect `focus_row_rect` indexes into,
/// so `draw_list` can position the focused-row tile without re-rendering the header.
pub fn list_modal_content_rect(card: Rect, fonts: &Fonts, subtitle: &str, row_count: usize) -> Rect {
    let header_end = modal_header_end_y(fonts.label, fonts.value, card, subtitle);
    let rows_h = (row_count as i32 * (SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP)).max(0) as u32;
    Rect::new(
        card.x() + SIDE_PAD,
        header_end + HEADER_GAP,
        card.width().saturating_sub(SIDE_PAD as u32 * 2),
        rows_h,
    )
}

/// Render entire list modal unfocused (header + all rows).
pub fn render_list_modal(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    card: Rect,
    title: &str,
    subtitle: &str,
    rows: &[FocusRow],
) -> Result<()> {
    draw_modal_header(
        painter,
        text_cache,
        fonts.label,
        fonts.value,
        card,
        title,
        WHITE,
        subtitle,
        MUTED,
    )?;
    let content = list_modal_content_rect(card, fonts, subtitle, rows.len());
    // `usize::MAX` = nothing focused here; see the module docs.
    draw_focus_rows(painter, text_cache, fonts, rows, usize::MAX, None, content)
}

/// Navigate within a list, wrapping. Returns true if focus moved.
pub fn list_nav(focused: &mut usize, len: usize, ev: MenuEvent) -> bool {
    if len == 0 {
        return false;
    }
    match ev {
        MenuEvent::Up => {
            *focused = if *focused == 0 { len - 1 } else { *focused - 1 };
            true
        }
        MenuEvent::Down => {
            *focused = (*focused + 1) % len;
            true
        }
        _ => false,
    }
}
