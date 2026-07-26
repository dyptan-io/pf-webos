//! Rasterized-once tile sources for the GPU compositor.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::ttf::Font;
use tiny_skia::Pixmap;

// ------------------------------------------------------------------ GPU tiles --
// The compositor path (see `compositor.rs` + `App::prepare_tiles`): widgets are
// rasterized by tiny-skia into standalone padded tiles ONCE (keeping the AA/soft
// shadow look), then composed per frame by the GPU — position, scroll, the focus
// pop's scale, and fades are all texture-copy parameters, not re-rasterization.

/// Transparent padding around a card tile so its drop shadow (dx 3 / dy 5 /
/// blur 14) fits inside the tile instead of clipping at its edge.
pub const CARD_TILE_PAD: i32 = 20;

/// One grid card pre-composited into its own padded transparent tile, drawn
/// unfocused — the focused look is the GPU scaling this same tile up slightly
/// plus the shared [`render_focus_ring_tile`] composited over it.
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
/// lottiefiles.com/free-animation/purple-spinner-peYjszu1K5, exported as
/// `assets/logo/punktfunk-spinner.gif`), decoded at build time by `build.rs`'s
/// `build_spinner_frames` — no GIF/LZW decode on-device. Layout: `u32` width,
/// `u32` height, `u32` frame count, then per frame a `u32` delay in milliseconds
/// followed by `width * height * 4` bytes of premultiplied RGBA8, at the GIF's
/// native size (no resize).
static SPINNER_FRAMES_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/spinner_frames.bin"));

/// One pre-decoded spinner frame and how long it stays on screen.
struct SpinnerFrame {
    pixmap: Pixmap,
    delay: std::time::Duration,
}

/// Parses `SPINNER_FRAMES_BLOB` once. Just slicing already-decoded bytes into
/// `Pixmap`s — no image decode, no resize, cheap enough to not need a background
/// warm-up thread.
fn spinner_frames() -> &'static [SpinnerFrame] {
    static FRAMES: std::sync::OnceLock<Vec<SpinnerFrame>> = std::sync::OnceLock::new();
    FRAMES.get_or_init(|| {
        let mut data = SPINNER_FRAMES_BLOB;
        let Some((width_bytes, rest)) = data.split_first_chunk::<4>() else {
            return Vec::new();
        };
        data = rest;
        let Some((height_bytes, rest)) = data.split_first_chunk::<4>() else {
            return Vec::new();
        };
        data = rest;
        let Some((count_bytes, rest)) = data.split_first_chunk::<4>() else {
            return Vec::new();
        };
        data = rest;
        let width = u32::from_le_bytes(*width_bytes);
        let height = u32::from_le_bytes(*height_bytes);
        let count = u32::from_le_bytes(*count_bytes) as usize;
        let Some(size) = tiny_skia::IntSize::from_wh(width, height) else {
            return Vec::new();
        };
        let frame_bytes = (width * height * 4) as usize;
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            let Some((delay_bytes, rest)) = data.split_first_chunk::<4>() else {
                break;
            };
            let delay = std::time::Duration::from_millis(u32::from_le_bytes(*delay_bytes).into());
            if rest.len() < frame_bytes {
                break;
            }
            let (pixels, rest) = rest.split_at(frame_bytes);
            data = rest;
            let Some(pixmap) = Pixmap::from_vec(pixels.to_vec(), size) else {
                break;
            };
            frames.push(SpinnerFrame { pixmap, delay });
        }
        frames
    })
}

/// The spinner frame showing `phase` seconds after the spinner started
/// (`App::spinner_since`), looped over the animation's total duration. Frames are
/// already decoded (build time, see `SPINNER_FRAMES_BLOB`), so this is just an
/// index pick + one small `Pixmap` copy — cheap enough to rebuild every tick the
/// spinner is shown, no per-frame GPU texture caching needed. `1x1` (invisible)
/// if the embedded blob is somehow empty.
pub fn render_spinner_tile(phase: f32) -> Painter {
    let frames = spinner_frames();
    let Some(first) = frames.first() else {
        return Painter::new(1, 1);
    };
    let total: std::time::Duration = frames.iter().map(|f| f.delay).sum();
    let mut elapsed = std::time::Duration::from_secs_f32(phase.max(0.0)).as_nanos() % total.as_nanos().max(1);
    let mut frame = first;
    for f in frames {
        if elapsed < f.delay.as_nanos() {
            frame = f;
            break;
        }
        elapsed -= f.delay.as_nanos();
    }
    let mut p = Painter::new(frame.pixmap.width(), frame.pixmap.height());
    p.draw_pixmap(0, 0, &frame.pixmap);
    p
}

/// Transparent padding around the focus-ring tile (the ring's outer glow pass
/// sits 6px out + stroke width).
pub const FOCUS_RING_PAD: i32 = 12;

/// The focus-ring glow for a `(w, h)` card, in its own transparent tile — one
/// shared tile serves every card (they're all the same size), scaled by the GPU
/// together with the focused card and faded in via texture alpha.
pub fn render_focus_ring_tile(w: u32, h: u32) -> Painter {
    let pad = FOCUS_RING_PAD;
    let mut p = Painter::new(w + 2 * pad as u32, h + 2 * pad as u32);
    draw_focus_ring(&mut p, Rect::new(pad, pad, w, h), CARD_RADIUS);
    p
}

/// Transparent padding around a focused-row tile, generous enough for a
/// row's shadow bleed (~20px) plus, for rows that still bake their own ~2%
/// inflate (sidebar rows — see [`draw_selectable`]), that growth too.
/// Settings rows animate their zoom via GPU scale instead (see
/// [`draw_selectable_fixed`]) and so don't need the second allowance, but
/// share this same constant for simplicity.
pub const ROW_TILE_PAD: i32 = 28;

/// Sidebar row `index`, focused, as its own padded transparent tile — composited
/// by the GPU over the focus-free sidebar layer. Mirrors `draw_sidebar`'s row
/// order (hosts, "+ Add host", bottom-pinned "Settings"); all three row kinds
/// share one rect size, so the tile dimensions are constant.
/// `menu_focused` picks out the row's ⋯ actions button instead of the row body: both
/// states reuse this one tile (the row still renders focused either way), so moving
/// between a host and its actions button costs one small re-rasterize, not a modal.
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

/// A worst-case stat line (max resolution + longest codec/HDR tag), measured to
/// fix the overlay's width — see `render_stats_overlay_tile`.
pub const STATS_OVERLAY_REF_LINE: &str = "3840x2160@120 HEVC HDR";

/// The in-stream stats overlay panel: a translucent brand-dark rounded card with
/// one line of text per stat. Rebuilt at the overlay's ~2Hz refresh with a
/// THROWAWAY `TextCache` — the numeric lines change every refresh, so a
/// persistent cache would only accumulate dead entries for the whole stream's
/// duration.
///
/// Width is FIXED — measured from `STATS_OVERLAY_REF_LINE` plus a safety margin,
/// not from the live content — so the right-anchored panel keeps a constant left
/// edge instead of jittering horizontally as the numbers change digit count.
/// Lines are ellipsized to the inner width as a further safety, so an unexpectedly
/// long line can never overflow the card.
pub fn render_stats_overlay_tile(font: &Font, lines: &[String]) -> Result<Painter> {
    let pad = 18i32;
    let safety = 16u32; // extra slack past the reference width, so nothing touches the edge
    let line_h = font.height() + 6;
    let inner_w = font.size_of(STATS_OVERLAY_REF_LINE).map_or(0, |(w, _)| w) + safety;
    let w = inner_w + 2 * pad as u32;
    let h = (lines.len() as i32 * line_h + 2 * pad) as u32;
    let mut p = Painter::new(w.max(1), h.max(1));
    let mut tc = TextCache::new();
    p.fill_rounded_rect(Rect::new(0, 0, w, h), 14, Color::RGBA(0x14, 0x10, 0x1f, 0xd2));
    for (i, line) in lines.iter().enumerate() {
        // First line (mode/codec header) pops; the measurements below are muted.
        let color = if i == 0 { WHITE } else { MUTED };
        let clipped = ellipsize(font, line, inner_w);
        draw_text(&mut p, &mut tc, font, &clipped, pad, pad + i as i32 * line_h, color)?;
    }
    Ok(p)
}

/// The Stop/Cancel button pair — shared by the shell and the
/// focused-button tile (`render_confirm_button_tile`), so their
/// `ConfirmButton` data can't drift apart.
pub fn disconnect_dialog_buttons() -> [ConfirmButton<'static>; 2] {
    [
        // Echoes the question's own verb ("Stop streaming?"), the same way the Forget
        // confirmation's button echoes its title.
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

/// The disconnect dialog's card rect and its button row's rect — the one
/// place this layout lives, shared by `render_disconnect_dialog_shell` and
/// `main.rs` (which needs the button rect, without drawing, to position the
/// focused-button tile).
pub fn disconnect_dialog_layout(screen_w: u32, screen_h: u32, font_label: &Font) -> (Rect, Rect) {
    // Wide enough for the buttons' own labels, and never narrower than the 34%
    // this has always been. The labels are real words at a font size that scales
    // with the panel, while 34% of the screen knows nothing about either — which
    // is how "Stop streaming" ended up ellipsized here at 1080p but not at 4K.
    // Capped at 90% so a hypothetical very long label still leaves a margin
    // rather than running to the screen edges.
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

/// The in-stream disconnect-confirmation dialog's shell: card, title, and
/// both confirm buttons unfocused — full-screen sized like `App`'s
/// `Tile::Modal` so `main.rs` can place it at `(0, 0)`. No backdrop dim baked
/// in — `main.rs` draws that as its own compositor `Fill`, same as
/// `App::draw_list` scrims `Tile::Modal`. The actually focused button
/// composites on top as its own small, zoom-animated tile (see
/// `render_confirm_button_tile`) — same shell/focus-tile split as every other
/// modal in the app.
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
