//! Focus rings, selectable cards, game-grid poster card.
use super::*;
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use tiny_skia::Pixmap;

/// Card corner radius (softened from moonlight-tv's ~2px).
pub const CARD_RADIUS: i32 = 10;
pub const MODAL_RADIUS: i32 = 20;

/// Approximate moonlight-tv's 2% focus zoom by inflating rect from center.
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

/// Soft drop shadow matching moonlight-tv's card look.
pub fn draw_card_shadow(painter: &mut Painter, rect: Rect, radius: i32) {
    painter.fill_shadow(rect, radius, 3.0, 5.0, SHADOW_BLUR, 0x60);
}

/// Focus ring: outline offset outward (moonlight-tv style). Used only for game grid selection.
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

/// Draw text-entry card (PIN/IP boxes); always visible, zoom when focused.
pub fn draw_card(painter: &mut Painter, rect: Rect, focused: bool) -> Rect {
    let r = inflate(rect, focused);
    draw_card_shadow(painter, r, CARD_RADIUS);
    painter.fill_rounded_rect(r, CARD_RADIUS, SURFACE);
    r
}

/// Card painted only when focused (no background for unfocused). Used by rows/buttons.
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
        // Already stretched to this card size by `art::ArtLoader` (see
        // `art::resize_pixmap`) — a plain blit, not `draw_pixmap_scaled`. Falls back
        // to scaling if a pixmap ever arrives at some other size.
        Some(pixmap) if pixmap.width() == r.width() && pixmap.height() == r.height() => {
            painter.draw_pixmap(r.x(), r.y(), pixmap);
        }
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
        r.x(),
        r.y() + r.height() as i32 - strip_h,
        r.width(),
        strip_h.max(0) as u32,
    );
    painter.fill_frosted_rect(strip, 0, Color::RGBA(0x00, 0x00, 0x00, 0x68), 6);
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
