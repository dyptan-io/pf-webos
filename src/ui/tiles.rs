//! Rasterized-once tile sources for the GPU compositor.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::ttf::Font;
use tiny_skia::Pixmap;

// ---------------------------------GPU tiles-----------------------------------
// The compositor path (see `compositor.rs` + `App::prepare_tiles`): widgets are
// rasterized by tiny-skia into standalone padded tiles ONCE (keeping the AA/soft
// shadow look), then composed per frame by the GPU — position, scroll, the focus
// pop's scale, and fades are all texture-copy parameters, not re-rasterization.

/// Transparent padding around a card tile so its drop shadow (dx 3 / dy 5 /
/// blur 14) fits inside the tile instead of clipping at its edge.
pub const CARD_TILE_PAD: i32 = 20;

/// Grid card as padded tile (unfocused). GPU scales + composites focus ring.
pub fn render_card_tile(
    text_cache: &mut TextCache,
    fonts: &Fonts,
    card_w: u32,
    card_h: u32,
    title: &str,
    art: Option<&Pixmap>,
) -> Result<Painter> {
    let pad = CARD_TILE_PAD;
    let mut p = Painter::new(card_w + 2 * pad as u32, card_h + 2 * pad as u32);
    draw_poster_card(
        &mut p,
        text_cache,
        fonts,
        Rect::new(pad, pad, card_w, card_h),
        title,
        art,
        false,
    )?;
    Ok(p)
}

/// The animated loading spinner (purple, from
/// lottiefiles.com/free-animation/purple-spinner-peYjszu1K5, embedded as
/// `assets/logo/punktfunk-spinner.gif`).
static SPINNER_GIF_BYTES: &[u8] = include_bytes!("../../assets/logo/punktfunk-spinner.gif");

/// One decoded spinner frame (straight RGBA8) and how long it stays on screen.
pub struct SpinnerFrame {
    pub width: u32,
    pub height: u32,
    pub delay: std::time::Duration,
    pub pixels: Vec<u8>,
}

/// Decodes `SPINNER_GIF_BYTES` once into pre-decoded straight RGBA8 frames.
pub fn spinner_frames() -> &'static [SpinnerFrame] {
    static FRAMES: std::sync::OnceLock<Vec<SpinnerFrame>> = std::sync::OnceLock::new();
    FRAMES.get_or_init(|| {
        use image::{codecs::gif::GifDecoder, AnimationDecoder};
        let Ok(decoder) = GifDecoder::new(std::io::Cursor::new(SPINNER_GIF_BYTES)) else {
            return Vec::new();
        };
        let Ok(raw_frames) = decoder.into_frames().collect::<image::ImageResult<Vec<_>>>() else {
            return Vec::new();
        };
        let mut frames = Vec::with_capacity(raw_frames.len());
        for frame in raw_frames {
            let (w, h) = frame.buffer().dimensions();
            let (numer, denom) = frame.delay().numer_denom_ms();
            let raw_delay = numer.checked_div(denom).unwrap_or(0);
            // WHY: clamp to ~30 FPS min to avoid busy-looping the render thread.
            let delay_ms = if raw_delay < 20 { 33 } else { raw_delay };
            let delay = std::time::Duration::from_millis(u64::from(delay_ms));
            let pixels = frame.into_buffer().into_raw();
            frames.push(SpinnerFrame {
                width: w,
                height: h,
                delay,
                pixels,
            });
        }
        frames
    })
}

/// Returns `SpinnerFrame` at index `idx`, or `None` when the GIF decoded to zero frames.
pub fn spinner_frame(idx: usize) -> Option<&'static SpinnerFrame> {
    spinner_frames().get(idx)
}

/// Returns the frame index and reference for `phase` seconds after the spinner started.
/// Falls back to a 1×1 transparent dummy if the GIF decoded to zero frames.
pub fn spinner_frame_at(phase: f32) -> (usize, &'static SpinnerFrame) {
    let frames = spinner_frames();
    if let Some(first) = frames.first() {
        let total: std::time::Duration = frames.iter().map(|f| f.delay).sum();
        let mut elapsed = std::time::Duration::from_secs_f32(phase.max(0.0)).as_nanos() % total.as_nanos().max(1);
        for (idx, f) in frames.iter().enumerate() {
            if elapsed < f.delay.as_nanos() {
                return (idx, f);
            }
            elapsed -= f.delay.as_nanos();
        }
        (0, first)
    } else {
        static DUMMY: std::sync::OnceLock<SpinnerFrame> = std::sync::OnceLock::new();
        let dummy = DUMMY.get_or_init(|| SpinnerFrame {
            width: 1,
            height: 1,
            delay: std::time::Duration::from_millis(100),
            pixels: vec![0, 0, 0, 0],
        });
        (0, dummy)
    }
}

/// Transparent padding around the focus-ring tile (the ring's outer glow pass
/// sits 6px out + stroke width).
pub const FOCUS_RING_PAD: i32 = 12;

/// Focus-ring glow as shared tile (all cards same size). GPU scales + fades.
pub fn render_focus_ring_tile(w: u32, h: u32) -> Painter {
    let pad = FOCUS_RING_PAD;
    let mut p = Painter::new(w + 2 * pad as u32, h + 2 * pad as u32);
    draw_focus_ring(&mut p, Rect::new(pad, pad, w, h), CARD_RADIUS);
    p
}

/// Diameter of the pinned badge composited over the focused grid/pinned
/// card's top-right corner (see `Tile::PinBadge`).
pub const PIN_BADGE_SIZE: u32 = 28;

/// Pinned badge: dark disc with PIN icon. Single shared tile.
pub fn render_pin_badge_tile(text_cache: &mut TextCache, icon_font: &Font) -> Result<Painter> {
    let d = PIN_BADGE_SIZE;
    let mut p = Painter::new(d, d);
    let c = d as f32 / 2.0;
    p.fill_circle(c, c, c, Color::RGBA(0x00, 0x00, 0x00, 0x70));
    let icon = (d as f32 * 0.6) as u32;
    let icon_rect = Rect::new(((d - icon) / 2) as i32, ((d - icon) / 2) as i32, icon, icon);
    draw_icon(&mut p, text_cache, icon_font, icon_rect, ICON_PIN, MUTED)?;
    Ok(p)
}

/// Padding for row tile shadow + sidebar inflate. Settings rows use GPU scale.
pub const ROW_TILE_PAD: i32 = 28;

/// Focused sidebar row as padded tile. `menu_focused` flags the actions button.
/// Both button states reuse one tile; moving between them costs one re-rasterize.
pub fn render_focused_row_tile(
    text_cache: &mut TextCache,
    fonts: &Fonts,
    entries: &[HostEntry],
    index: usize,
    menu_focused: bool,
    online: Option<bool>,
) -> Result<Painter> {
    let pad = ROW_TILE_PAD;
    let base = sidebar_row_rect(0);
    let rect = Rect::new(pad, pad, base.width(), base.height());
    let mut p = Painter::new(base.width() + 2 * pad as u32, base.height() + 2 * pad as u32);
    if let Some(entry) = entries.get(index) {
        draw_host_row(
            &mut p,
            text_cache,
            fonts,
            rect,
            entry.name(),
            entry.is_paired(),
            true,
            false,
            menu_focused,
            online,
        )?;
    } else if index == entries.len() {
        draw_utility_row(&mut p, text_cache, fonts, rect, "+ Add host", true)?;
    } else {
        draw_utility_row(&mut p, text_cache, fonts, rect, "Settings", true)?;
    }
    Ok(p)
}

/// A single line of text as its own tight transparent tile.
pub fn render_text_tile(text_cache: &mut TextCache, font: &Font, text: &str, color: Color) -> Result<Painter> {
    let (w, h) = font.size_of(text).unwrap_or((1, 1));
    let mut p = Painter::new(w.max(1), h.max(1));
    draw_text(&mut p, text_cache, font, text, 0, 0, color)?;
    Ok(p)
}

/// A wrapped text block as its own transparent tile (`max_w` wide, as tall as
/// its wrapped line count).
pub fn render_wrapped_text_tile(
    text_cache: &mut TextCache,
    font: &Font,
    text: &str,
    max_w: u32,
    color: Color,
    line_gap: i32,
) -> Result<Painter> {
    let line_h = font.height() + line_gap;
    let lines = wrap_text(font, text, max_w).len().max(1) as u32;
    let mut p = Painter::new(max_w.max(1), lines * line_h.max(1) as u32);
    draw_text_wrapped(&mut p, text_cache, font, text, 0, 0, max_w, color, line_gap)?;
    Ok(p)
}

/// A worst-case stat line, measured to fix the overlay's width — see
/// `render_stats_overlay_tile`. The Drop/FEC/hold/buf line is the widest of the
/// bunch once all four counters hit multiple digits.
pub const STATS_OVERLAY_REF_LINE: &str = "Drop 99  FEC 99  hold yes  buf 99";

/// In-stream stats overlay: translucent card with stat lines + Green-button hint.
/// Width is FIXED (measured from `STATS_OVERLAY_REF_LINE`) so panel doesn't jitter
/// as numbers change digit count. `lines[0]` is highlighted; rest muted.
pub fn render_stats_overlay_tile(font: &Font, caption_font: &Font, lines: &[String], hint: &str) -> Result<Painter> {
    let pad = 18i32;
    let safety = 16u32;
    let line_h = font.height() + 6;
    let hint_h = caption_font.height() + 8;
    let inner_w = font.size_of(STATS_OVERLAY_REF_LINE).map_or(0, |(w, _)| w) + safety;
    let w = inner_w + 2 * pad as u32;
    let h = (lines.len() as i32 * line_h + hint_h + 2 * pad) as u32;
    let mut p = Painter::new(w.max(1), h.max(1));
    let mut tc = TextCache::new();
    p.fill_rounded_rect(Rect::new(0, 0, w, h), 14, Color::RGBA(0x14, 0x10, 0x1f, 0x90));
    for (i, line) in lines.iter().enumerate() {
        let color = if i == 0 { WHITE } else { MUTED };
        let clipped = ellipsize(font, line, inner_w);
        draw_text(&mut p, &mut tc, font, &clipped, pad, pad + i as i32 * line_h, color)?;
    }
    let hint_y = pad + lines.len() as i32 * line_h + (hint_h - caption_font.height());
    let hint_w = caption_font.size_of(hint).map_or(0, |(w, _)| w) as i32;
    let hint_x = pad + (w as i32 - 2 * pad - hint_w) / 2;
    draw_text(&mut p, &mut tc, caption_font, hint, hint_x, hint_y, MUTED)?;
    Ok(p)
}

/// Number of lines shown in the log-tail overlay.
pub const LOG_OVERLAY_LINES: usize = 12;

/// Color for log line by level prefix; errors/warnings highlighted to stand out.
fn log_line_color(line: &str) -> Color {
    match line.split_whitespace().next() {
        Some("ERROR") => ERROR_RED,
        Some("WARN") => WARNING,
        Some("INFO") => WHITE,
        _ => MUTED,
    }
}

/// Full-width log-tail at screen bottom (all screens, unlike stats overlay).
pub fn render_log_overlay_tile(font: &Font, screen_w: u32, lines: &[String]) -> Result<Painter> {
    let pad = 14i32;
    let line_h = font.height() + 4;
    let inner_w = screen_w.saturating_sub(2 * pad as u32);
    let h = (lines.len().max(1) as i32 * line_h + 2 * pad).max(1) as u32;
    let mut p = Painter::new(screen_w.max(1), h);
    let mut tc = TextCache::new();
    p.fill_rounded_rect(
        Rect::new(0, 0, screen_w.max(1), h),
        14,
        Color::RGBA(0x14, 0x10, 0x1f, 0x90),
    );
    for (i, line) in lines.iter().enumerate() {
        let clipped = ellipsize(font, line, inner_w);
        draw_text(&mut p, &mut tc, font, &clipped, pad, pad + i as i32 * line_h, log_line_color(line))?;
    }
    Ok(p)
}

/// Stop/Cancel button pair for shell and focused-button tile.
pub fn disconnect_dialog_buttons() -> [ConfirmButton<'static>; 2] {
    [
        ConfirmButton {
            icon: Some(ICON_CLOSE),
            label: "Stop streaming",
            color: ERROR_RED,
        },
        ConfirmButton {
            icon: None,
            label: "Cancel",
            color: WHITE,
        },
    ]
}

/// Disconnect dialog card + button row rects (shared with main.rs for layout).
pub fn disconnect_dialog_layout(screen_w: u32, screen_h: u32, font_label: &Font) -> (Rect, Rect) {
    // Width: at least 34%, enough for button labels, capped at 90% for margin.
    let needed = confirm_row_min_width(font_label, &disconnect_dialog_buttons()) + 64;
    let frac = (needed as f32 / screen_w.max(1) as f32).clamp(0.34, 0.90);
    let card = modal_card_rect(screen_w, screen_h, frac, 200);
    let content = Rect::new(
        card.x() + 32,
        card.y() + 36 + font_label.height() + 28,
        card.width().saturating_sub(64),
        72,
    );
    (card, content)
}

/// Disconnect dialog shell (full-screen): card + title + unfocused buttons.
/// Focused button composites on top as own small tile (shell/focus-tile split).
pub fn render_disconnect_dialog_shell(screen_w: u32, screen_h: u32, fonts: &Fonts) -> Result<Painter> {
    let mut p = Painter::new(screen_w, screen_h);
    let mut tc = TextCache::new();
    let (card, content) = disconnect_dialog_layout(screen_w, screen_h, fonts.label);
    draw_modal_card(&mut p, card);
    let title = "Stop streaming?";
    let (title_w, _) = fonts.label.size_of(title).unwrap_or((0, 0));
    let title_x = card.x() + (card.width() as i32 - title_w as i32) / 2;
    draw_text(&mut p, &mut tc, fonts.label, title, title_x, card.y() + 36, WHITE)?;
    draw_confirm_buttons(
        &mut p,
        &mut tc,
        fonts,
        content,
        &disconnect_dialog_buttons(),
        usize::MAX,
    )?;
    Ok(p)
}
