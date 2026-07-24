//! Focus rings, selectable cards, and the game-grid poster card.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use tiny_skia::Pixmap;


/// A slight softening of moonlight-tv's near-square (~2px) tile radius.
pub const CARD_RADIUS: i32 = 10;
pub const MODAL_RADIUS: i32 = 20;

/// Approximates moonlight-tv's 102%/99% focus/press zoom (a real `transform_zoom`
/// isn't worth a transform pipeline here) by inflating the drawn rect a few percent
/// from its own center when focused.
pub fn inflate(rect: Rect, focused: bool) -> Rect {
    if !focused {
        return rect;
    }
    let grow_w = ((rect.width() as f32) * 0.02).round() as i32;
    let grow_h = ((rect.height() as f32) * 0.02).round() as i32;
    Rect::new(
        rect.x() - grow_w,
        rect.y() - grow_h,
        rect.width() + 2 * grow_w as u32,
        rect.height() + 2 * grow_h as u32,
    )
}

/// A soft, real drop shadow (see [`Painter::fill_shadow`]) — matches the reference's
/// shadowed-card look.
pub fn draw_card_shadow(painter: &mut Painter, rect: Rect, radius: i32) {
    painter.fill_shadow(rect, radius, 3.0, 5.0, SHADOW_BLUR, 0x60);
}

/// moonlight-tv's focus cue is an outline ring offset outward from the tile, not a
/// filled/background change — bright accent blue, invisible unless focused. Two
/// passes at increasing offset/decreasing alpha approximate a soft glow. Only
/// `draw_poster_card` (game/Desktop grid selection) uses this — every other
/// selectable row/button relies on [`draw_selectable`]'s zoom, focus-only card,
/// and text-color change instead, per an explicit request to drop rings
/// everywhere except game selection.
pub fn draw_focus_ring(painter: &mut Painter, rect: Rect, radius: i32) {
    let passes = [(3, 0xff), (6, 0x60)];
    for (offset, alpha) in passes {
        let ring = Rect::new(
            rect.x() - offset,
            rect.y() - offset,
            rect.width() + 2 * offset as u32,
            rect.height() + 2 * offset as u32,
        );
        let color = Color::RGBA(ACCENT_BRIGHT.r, ACCENT_BRIGHT.g, ACCENT_BRIGHT.b, alpha);
        painter.stroke_rounded_rect(ring, radius + offset, color, 2.0);
    }
}

/// Draws a plain surface card for a text-entry field (PIN/IP digit boxes) — always
/// visible, so every slot reads as "a box you can fill in", not just the current
/// one — shadow and `SURFACE` fill, zoom-inflated slightly when focused. Returns
/// the (possibly zoom-inflated) rect actually drawn, so callers can center content
/// inside it. Selectable rows/buttons use [`draw_selectable`] instead, which only
/// paints the box when focused.
pub fn draw_card(painter: &mut Painter, rect: Rect, focused: bool) -> Rect {
    let r = inflate(rect, focused);
    draw_card_shadow(painter, r, CARD_RADIUS);
    painter.fill_rounded_rect(r, CARD_RADIUS, SURFACE);
    r
}

/// Same card as [`draw_card`], but only painted when focused — an unfocused
/// row/button has no background at all. Used by every selectable row/button
/// (sidebar, Wake, confirm) except settings rows, which use
/// [`draw_selectable_fixed`] instead (see its docs).
pub fn draw_selectable(painter: &mut Painter, rect: Rect, focused: bool) -> Rect {
    let r = inflate(rect, focused);
    if focused {
        draw_card_shadow(painter, r, CARD_RADIUS);
        painter.fill_rounded_rect(r, CARD_RADIUS, SURFACE);
    }
    r
}

/// Same as [`draw_selectable`] but never inflates: settings rows are
/// rasterized once at their literal size, and `app.rs`'s `draw_list` animates
/// the zoom-in itself by GPU-scaling the whole focused-row tile around its
/// center (same technique as the grid's card focus-pop) — a CPU-baked inflate
/// here would fight that, since the rasterized content would then need
/// re-rendering every animation frame instead of just repositioning.
pub fn draw_selectable_fixed(painter: &mut Painter, rect: Rect, focused: bool) {
    if focused {
        draw_card_shadow(painter, rect, CARD_RADIUS);
        painter.fill_rounded_rect(rect, CARD_RADIUS, SURFACE);
    }
}

/// A handful of muted hues for the poster-card placeholder tint (hash-selected per
/// title, not arbitrary RGB) — kept dark enough that white text stays legible.
pub const POSTER_TINTS: [Color; 6] = [
    Color::RGB(0x4a, 0x3a, 0x7d), // violet
    Color::RGB(0x35, 0x40, 0x6e), // indigo
    Color::RGB(0x6b, 0x3a, 0x68), // plum
    Color::RGB(0x57, 0x50, 0x93), // deep lavender
    Color::RGB(0x3a, 0x4a, 0x8c), // slate blue
    Color::RGB(0x7d, 0x4a, 0x5e), // mauve
];

pub fn tint_for(title: &str) -> Color {
    let hash = title
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(u32::from(b)));
    POSTER_TINTS[hash as usize % POSTER_TINTS.len()]
}

/// Draws one game/Desktop tile. `art`, when `Some` (a decoded cover, already
/// downscaled and premultiplied by `art.rs`), fills the whole card, same as
/// moonlight-tv's cover-image tiles; `None` falls back to a tinted placeholder +
/// initial letter (no real art fetched yet, or the host has none for this title).
/// Either way a bottom title strip overlays the art/tint, matching the reference's
/// always-present (ellipsized) title label.
pub fn draw_poster_card(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    rect: Rect,
    title: &str,
    art: Option<&Pixmap>,
    focused: bool,
) -> Result<()> {
    let r = inflate(rect, focused);
    draw_card_shadow(painter, r, CARD_RADIUS);

    let strip_h = (fonts.value.height() + 16).min(r.height() as i32 / 3);
    match art {
        Some(pixmap) => {
            painter.draw_pixmap_scaled(r, pixmap);
        }
        None => {
            painter.fill_rounded_rect(r, CARD_RADIUS, tint_for(title));
            let initial = title
                .chars()
                .find(|c| c.is_alphanumeric())
                .unwrap_or('?')
                .to_uppercase()
                .to_string();
            let (iw, ih) = fonts.title.size_of(&initial).unwrap_or((0, 0));
            let art_h = r.height() as i32 - strip_h;
            draw_text(
                painter,
                text_cache,
                fonts.title,
                &initial,
                r.x() + (r.width() as i32 - iw as i32) / 2,
                r.y() + (art_h - ih as i32) / 2,
                Color::RGBA(0xff, 0xff, 0xff, 0xa0),
            )?;
        }
    }

    let strip = Rect::new(
        r.x() + 2,
        r.y() + r.height() as i32 - strip_h,
        r.width().saturating_sub(4),
        strip_h.max(0) as u32,
    );
    painter.fill_rect(strip, Color::RGBA(0x00, 0x00, 0x00, 0x70));
    let label = ellipsize(fonts.value, title, strip.width().saturating_sub(16));
    draw_text(
        painter,
        text_cache,
        fonts.value,
        &label,
        strip.x() + 8,
        strip.y() + (strip.height() as i32 - fonts.value.height()) / 2,
        WHITE,
    )?;

    if focused {
        draw_focus_ring(painter, r, CARD_RADIUS);
    }
    Ok(())
}

