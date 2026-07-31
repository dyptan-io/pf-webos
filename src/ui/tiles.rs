//! Rasterized-once tile sources for the GPU compositor.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use crate::ui::render::Color;
use crate::ui::render::Rect;
use crate::ui::text_raster::{FontId, TextRaster};
use anyhow::Result;
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

/// Transparent padding around the focus-ring tile — must clear
/// `FOCUS_GLOW_BLUR`'s blur radius or the glow clips against the canvas edge.
pub const FOCUS_RING_PAD: i32 = 20;

/// Focus-ring glow as shared tile (all cards same size). GPU scales + fades.
pub fn render_focus_ring_tile(w: u32, h: u32) -> Painter {
    let pad = FOCUS_RING_PAD;
    let mut p = Painter::new(w + 2 * pad as u32, h + 2 * pad as u32);
    draw_focus_ring(&mut p, Rect::new(pad, pad, w, h), CARD_RADIUS);
    p
}

/// Transparent padding around the card-outline tile — just enough for the
/// stroke's own width/AA, not a blur radius like `FOCUS_RING_PAD`.
pub const CARD_OUTLINE_PAD: i32 = 4;

/// The focused card's crisp edge outline as a shared tile (all cards same
/// size), composited on top of the card art — see `draw_card_outline`.
pub fn render_card_outline_tile(w: u32, h: u32) -> Painter {
    let pad = CARD_OUTLINE_PAD;
    let mut p = Painter::new(w + 2 * pad as u32, h + 2 * pad as u32);
    draw_card_outline(&mut p, Rect::new(pad, pad, w, h));
    p
}

/// Diameter of the pinned badge composited over the focused grid/pinned
/// card's top-right corner (see `Tile::PinBadge`).
pub const PIN_BADGE_SIZE: u32 = 28;

/// Pinned badge: dark disc with PIN icon. Single shared tile.
pub fn render_pin_badge_tile(
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    icon_font: FontId,
) -> Result<Painter> {
    let d = PIN_BADGE_SIZE;
    let mut p = Painter::new(d, d);
    let c = d as f32 / 2.0;
    p.fill_circle(c, c, c, Color::RGBA(0x00, 0x00, 0x00, 0x70));
    let icon = (d as f32 * 0.6) as u32;
    let icon_rect = Rect::new(((d - icon) / 2) as i32, ((d - icon) / 2) as i32, icon, icon);
    draw_icon(&mut p, text_cache, raster, icon_font, icon_rect, ICON_PIN, MUTED)?;
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
pub fn render_text_tile(
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    font: FontId,
    text: &str,
    color: Color,
) -> Result<Painter> {
    let (w, h) = raster.measure(font, text);
    let mut p = Painter::new(w.max(1), h.max(1));
    draw_text(&mut p, text_cache, raster, font, text, 0, 0, color)?;
    Ok(p)
}

/// A wrapped text block as its own transparent tile (`max_w` wide, as tall as
/// its wrapped line count).
#[allow(clippy::too_many_arguments)]
pub fn render_wrapped_text_tile(
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    font: FontId,
    text: &str,
    max_w: u32,
    color: Color,
    line_gap: i32,
) -> Result<Painter> {
    let line_h = raster.height(font) + line_gap;
    let lines = wrap_text(raster, font, text, max_w).len().max(1) as u32;
    let mut p = Painter::new(max_w.max(1), lines * line_h.max(1) as u32);
    draw_text_wrapped(&mut p, text_cache, raster, font, text, 0, 0, max_w, color, line_gap)?;
    Ok(p)
}

/// Worst-case stat line used to lock overlay width.
pub const STATS_OVERLAY_REF_LINE: &str = "3840x2160@120 HEVC HDR Starfish";

/// In-stream stats overlay with fixed width and centered hint.
/// `lines[0]` is highlighted; remaining lines are muted.
pub fn render_stats_overlay_tile(
    raster: &dyn TextRaster,
    font: FontId,
    caption_font: FontId,
    lines: &[String],
    hint: &str,
) -> Result<Painter> {
    let pad = 18i32;
    let content_safety = 16u32;
    let line_h = raster.height(font) + 6;
    let caption_h = raster.height(caption_font);
    let hint_h = caption_h + 8;
    let line_count = lines.len() as i32;

    let inner_w = raster.measure(font, STATS_OVERLAY_REF_LINE).0 + content_safety;
    let w = inner_w + 2 * pad as u32;
    let h = (line_count * line_h + hint_h + 2 * pad) as u32;
    let content_w_i32 = w as i32 - 2 * pad;

    let mut p = Painter::new(w.max(1), h.max(1));
    let mut tc = TextCache::new();
    p.fill_rounded_rect(Rect::new(0, 0, w, h), 14, Color::RGBA(0x14, 0x10, 0x1f, 0x70));

    for (i, line) in lines.iter().enumerate() {
        let color = if i == 0 { WHITE } else { MUTED };
        let y = pad + i as i32 * line_h;
        draw_text(&mut p, &mut tc, raster, font, line, pad, y, color)?;
    }

    let hint_y = pad + line_count * line_h + (hint_h - caption_h);
    let hint_w = raster.measure(caption_font, hint).0 as i32;
    let hint_x = pad + (content_w_i32 - hint_w) / 2;
    draw_text(&mut p, &mut tc, raster, caption_font, hint, hint_x, hint_y, MUTED)?;
    Ok(p)
}

/// Number of lines shown in the log-tail overlay.
pub const LOG_OVERLAY_LINES: usize = 9;

/// Color for log line by level prefix; errors/warnings highlighted to stand out.
fn log_line_color(line: &str) -> Color {
    match line.split_whitespace().next() {
        Some("ERROR") => ERROR_RED,
        Some("WARN") => WARNING,
        Some("INFO") => WHITE,
        _ => MUTED,
    }
}

/// Left indent for a wrapped log line's 2nd+ row, so it reads as a continuation.
const LOG_OVERLAY_WRAP_INDENT: i32 = 20;

/// Full-width log-tail at screen bottom (all screens, unlike stats overlay) — a
/// constant left-to-right size regardless of content. Long lines word-wrap
/// instead of clipping, only once they'd actually reach the screen edge.
pub fn render_log_overlay_tile(
    raster: &dyn TextRaster,
    font: FontId,
    screen_w: u32,
    lines: &[String],
) -> Result<Painter> {
    let pad = 14i32;
    let line_h = raster.height(font) + 4;
    let inner_w = screen_w.saturating_sub(2 * pad as u32);
    // Narrowed by the indent so continuation rows fit too; first row just has slack.
    let wrap_w = inner_w.saturating_sub(LOG_OVERLAY_WRAP_INDENT as u32).max(1);
    let wrapped: Vec<(Vec<String>, Color)> = lines
        .iter()
        .map(|line| (wrap_text(raster, font, line, wrap_w), log_line_color(line)))
        .collect();
    let total_rows: usize = wrapped.iter().map(|(rows, _)| rows.len().max(1)).sum();
    let h = (total_rows.max(1) as i32 * line_h + 2 * pad).max(1) as u32;
    let mut p = Painter::new(screen_w.max(1), h);
    let mut tc = TextCache::new();
    p.fill_rounded_rect(
        Rect::new(0, 0, screen_w.max(1), h),
        14,
        Color::RGBA(0x14, 0x10, 0x1f, 0xb8),
    );
    let mut row = 0i32;
    for (wrapped_rows, color) in &wrapped {
        // Empty line wraps to zero rows — still reserve one so later lines don't creep up.
        if wrapped_rows.is_empty() {
            row += 1;
            continue;
        }
        for (i, text) in wrapped_rows.iter().enumerate() {
            let x = if i == 0 { pad } else { pad + LOG_OVERLAY_WRAP_INDENT };
            draw_text(&mut p, &mut tc, raster, font, text, x, pad + row * line_h, *color)?;
            row += 1;
        }
    }
    Ok(p)
}

/// Confirm dialog card rect: [`SIMPLE_MODAL_WIDTH_FRAC`] wide, height driven by the
/// wrapped subtitle plus room for the button row. Split from [`confirm_dialog_layout`]
/// so callers that only need the card (hit-testing the close button, positioning the
/// shell) skip the second subtitle wrap the button-row rect would cost.
pub fn confirm_dialog_card(screen_w: u32, screen_h: u32, fonts: &Fonts, subtitle: &str) -> Rect {
    simple_modal_card(screen_w, screen_h, |probe| {
        let header_end = modal_header_end_y(fonts.raster, fonts.label, fonts.value, probe, subtitle);
        (header_end + 32 + 72 + 32) as u32
    })
}

/// Confirm dialog card + button-row rects, sized exactly like the `App`'s simple modals
/// (forget host, send logs): [`SIMPLE_MODAL_WIDTH_FRAC`] wide, height driven by the wrapped
/// subtitle. Shared with `main.rs`'s in-stream/quit dialog so all four modals match.
pub fn confirm_dialog_layout(screen_w: u32, screen_h: u32, fonts: &Fonts, subtitle: &str) -> (Rect, Rect) {
    let card = confirm_dialog_card(screen_w, screen_h, fonts, subtitle);
    let after_subtitle_y = modal_header_end_y(fonts.raster, fonts.label, fonts.value, card, subtitle);
    let content = Rect::new(
        card.x() + 32,
        after_subtitle_y + 32,
        card.width().saturating_sub(64),
        72,
    );
    (card, content)
}

/// Button index under `(x, y)` within a confirm dialog's button-row `content` rect, or
/// `None` off both buttons — the shared hit-test the App confirm modals' hover arms and
/// the in-stream Disconnect dialog use so pointer focus behaves identically.
pub fn confirm_button_at(content: Rect, x: i32, y: i32) -> Option<usize> {
    (0..2).find(|&i| confirm_button_rect(content, i).contains_point((x, y)))
}

/// Confirm dialog shell (full-screen tile): the same card + close (X) + header + unfocused
/// buttons that `App::draw_modal_shell`/`ui::draw_modal_header` paint for the forget-host and
/// send-logs modals — replicated here because the streaming/quit loops have no `App`. The
/// focused button composites on top as its own small tile (shell/focus-tile split).
pub fn render_confirm_dialog_shell(
    screen_w: u32,
    screen_h: u32,
    fonts: &Fonts,
    title: &str,
    subtitle: &str,
    buttons: &[ConfirmButton; 2],
) -> Result<Painter> {
    let mut p = Painter::new(screen_w, screen_h);
    let mut tc = TextCache::new();
    let (card, content) = confirm_dialog_layout(screen_w, screen_h, fonts, subtitle);
    draw_modal_card(&mut p, card);
    // These dialogs are remote/controller-driven (no local pointer over the overlay), so the
    // X is a visual affordance only — always in the unhovered color.
    draw_icon(
        &mut p,
        &mut tc,
        fonts.raster,
        fonts.icon,
        modal_close_rect(card),
        ICON_CLOSE,
        MUTED,
    )?;
    draw_modal_header(
        &mut p,
        &mut tc,
        fonts.raster,
        fonts.label,
        fonts.value,
        card,
        title,
        WHITE,
        subtitle,
        MUTED,
    )?;
    draw_confirm_buttons(&mut p, &mut tc, fonts, content, buttons, usize::MAX)?;
    Ok(p)
}
