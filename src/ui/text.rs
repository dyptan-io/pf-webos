//! Text/icon cache and drawing (Geist + icon font). Rasterization itself goes
//! through `TextRaster` — this module never touches `sdl2::ttf`.
use super::*;
use crate::ui::render::Color;
use crate::ui::render::Rect;
use crate::ui::text_raster::{FontId, TextRaster};
use anyhow::Result;
use std::collections::HashMap;
use tiny_skia::{IntSize, Pixmap};

/// Bundled Geist family (punktfunk brand font); embedded so no asset staging needed.
pub static GEIST_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Geist-Regular.otf");
pub static GEIST_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Geist-Medium.otf");
pub static GEIST_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Geist-SemiBold.otf");

/// Geist weight to load (Bold unembedded; add variant if needed).
#[derive(Clone, Copy)]
pub enum FontWeight {
    Regular,
    Medium,
    SemiBold,
}

/// App UI fonts: a `TextRaster` plus which loaded font each logical role maps to.
pub struct Fonts<'a> {
    pub raster: &'a dyn TextRaster,
    pub label: FontId,
    pub value: FontId,
    pub title: FontId,
    pub icon: FontId,
    /// Smallest weight (stats overlay Green-button hint).
    pub caption: FontId,
}

/// Icon font bytes, embedded at compile time (no asset staging or runtime path needed).
pub static ICON_FONT_BYTES: &[u8] = include_bytes!("../../assets/icons/MaterialIcons-subset.ttf");

/// Punktfunk logo (rasterized at sidebar size, 1:1 no scaling). See assets/logo/NOTICE.md.
pub static LOGO_PNG: &[u8] = include_bytes!("../../assets/logo/logo-sidebar.png");

/// Decode embedded logo once, lazily (premultiplied, ready to composite). None if PNG invalid.
pub fn logo_pixmap() -> Option<&'static Pixmap> {
    static LOGO: std::sync::OnceLock<Option<Pixmap>> = std::sync::OnceLock::new();
    LOGO.get_or_init(|| {
        let decoded = image::load_from_memory(LOGO_PNG).ok()?;
        let rgba = decoded.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        let mut buf = rgba.into_raw();
        premultiply_rgba(&mut buf);
        Pixmap::from_vec(buf, IntSize::from_wh(w, h)?)
    })
    .as_ref()
}

/// Caches rasterized-text `Pixmap`s across frames, keyed by the exact
/// `(text, color, font)` that produced them. Without this, `draw_text` re-rasterized
/// (freetype glyph lookup + blend + premultiply) on *every* call — and every draw
/// function in this module is called on every render tick (the pre-stream UI loop
/// runs at ~60fps), so a static label like "Settings" paid that cost 60 times a
/// second for pixels that never changed. Entry count is naturally bounded by this
/// app's own content (a handful of static labels, a bounded set of settings values,
/// one row per known host/game) — no eviction needed; see module docs if that
/// assumption ever stops holding.
pub struct TextCache {
    entries: HashMap<(String, u32, FontId), Pixmap>,
}

impl TextCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn key(font: FontId, text: &str, color: Color) -> (String, u32, FontId) {
        let packed_color = u32::from_be_bytes([color.r, color.g, color.b, color.a]);
        (text.to_string(), packed_color, font)
    }

    /// Returns the cached `Pixmap` for `(font, text, color)`, rasterizing (and
    /// caching) it first if this is the first time this exact combination has
    /// been drawn.
    fn get_or_create(&mut self, raster: &dyn TextRaster, font: FontId, text: &str, color: Color) -> Result<&Pixmap> {
        let key = Self::key(font, text, color);
        if !self.entries.contains_key(&key) {
            let pixmap = raster.rasterize(font, text, color)?;
            self.entries.insert(key.clone(), pixmap);
        }
        Ok(self.entries.get(&key).expect("just inserted"))
    }
}

impl Default for TextCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Renders one line of text left-aligned at `(x, y)` (top-left), returning its
/// width. `text_cache` (see [`TextCache`]) makes repeat calls with the same
/// `(font, text, color)` — the common case, since most on-screen text is static
/// from one frame to the next — cheap: no re-rasterization, no re-premultiplying.
#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    font: FontId,
    text: &str,
    x: i32,
    y: i32,
    color: Color,
) -> Result<u32> {
    if text.is_empty() {
        return Ok(0);
    }
    let pixmap = text_cache.get_or_create(raster, font, text, color)?;
    let width = pixmap.width();
    painter.draw_pixmap(x, y, pixmap);
    Ok(width)
}

/// Renders one line of text WITHOUT touching [`TextCache`] — for text that is
/// unique per line and scrolled past once, where caching is pure loss.
///
/// [`TextCache`] is deliberately unbounded (see its docs: entry count is bounded by
/// the app's own content — a handful of labels, one row per host/game). The About
/// screen's licence wall breaks that assumption badly: `THIRD-PARTY-NOTICES.txt` is
/// ~10,000 distinct lines, so scrolling the whole document through a cached
/// `draw_text` would leave ~10,000 rasterized `Pixmap`s resident for the rest of the
/// process — on a TV with no eviction path. These lines are drawn at most a couple of
/// times each (once per scroll position that shows them), so rasterizing fresh is both
/// cheaper overall and bounded in memory.
pub fn draw_text_uncached(
    painter: &mut Painter,
    raster: &dyn TextRaster,
    font: FontId,
    text: &str,
    x: i32,
    y: i32,
    color: Color,
) -> Result<u32> {
    if text.is_empty() {
        return Ok(0);
    }
    let pixmap = raster.rasterize(font, text, color)?;
    let width = pixmap.width();
    painter.draw_pixmap(x, y, &pixmap);
    Ok(width)
}

/// Draws one icon glyph (one of the `ICON_*` constants above) from the bundled icon
/// font, scaled to fill `rect` — the same `TextCache` that caches on-screen text
/// caches these too (a font id plus the glyph string is already a unique, stable
/// cache key — see [`TextCache`] — so a second cache wasn't needed just because this
/// one holds icons instead of words).
pub fn draw_icon(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    icon_font: FontId,
    rect: Rect,
    glyph: &str,
    color: Color,
) -> Result<()> {
    let pixmap = text_cache.get_or_create(raster, icon_font, glyph, color)?;
    painter.draw_pixmap_scaled(rect, pixmap);
    Ok(())
}

/// Truncates `text` with a trailing "…" so it fits within `max_w` pixels in `font`
/// (moonlight-tv scroll-marquees long titles on focus instead — see the module docs
/// on why this client keeps it simple).
pub fn ellipsize(raster: &dyn TextRaster, font: FontId, text: &str, max_w: u32) -> String {
    if raster.measure(font, text).0 <= max_w {
        return text.to_string();
    }
    let mut s: Vec<char> = text.chars().collect();
    while !s.is_empty() {
        s.pop();
        let candidate: String = s.iter().collect::<String>() + "…";
        if raster.measure(font, &candidate).0 <= max_w {
            return candidate;
        }
    }
    "…".to_string()
}

/// Greedily word-wraps `text` into lines no wider than `max_w` px in `font` — for modal
/// copy that's a full sentence or two (status/explanation text), unlike `ellipsize`'s
/// single-line truncation for card titles.
///
/// Tracks the running line width instead of re-measuring the whole (growing) line on
/// every word — the original did a full-prefix measure each time, which is O(line
/// length) per word and so O(n²) over a line, cheap for a sentence but the dominant
/// cost of wrapping About's ~10,000-line document on open (see `ui::wrap_document`).
/// Assumes negligible word-to-word kerning at the space boundary, same as every other
/// width-budget calculation in this UI already does.
pub fn wrap_text(raster: &dyn TextRaster, font: FontId, text: &str, max_w: u32) -> Vec<String> {
    let space_w = raster.measure(font, " ").0;
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0u32;
    for word in text.split_whitespace() {
        let word_w = raster.measure(font, word).0;
        let candidate_w = if current.is_empty() {
            word_w
        } else {
            current_w + space_w + word_w
        };
        if current.is_empty() || candidate_w <= max_w {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            current_w = candidate_w;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_w = word_w;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Draws `text` word-wrapped to `max_w` (see [`wrap_text`]), one line per
/// `raster.height(font) + line_gap`, starting at `(x, y)`. Returns the y position just
/// past the last line, so callers can stack more content beneath it without having to
/// guess how many lines it wrapped to.
#[allow(clippy::too_many_arguments)]
pub fn draw_text_wrapped(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    font: FontId,
    text: &str,
    x: i32,
    y: i32,
    max_w: u32,
    color: Color,
    line_gap: i32,
) -> Result<i32> {
    let mut cursor_y = y;
    for line in wrap_text(raster, font, text, max_w) {
        draw_text(painter, text_cache, raster, font, &line, x, cursor_y, color)?;
        cursor_y += raster.height(font) + line_gap;
    }
    Ok(cursor_y)
}

/// The pure geometry `draw_modal_header` and `modal_header_end_y` share:
/// `(text_x, subtitle_y, max_w)` — the one place it's computed, so the two
/// can never drift apart.
pub fn modal_header_geometry(raster: &dyn TextRaster, title_font: FontId, card: Rect) -> (i32, i32, u32) {
    let text_x = card.x() + 32;
    let title_y = card.y() + 28;
    let subtitle_y = title_y + raster.height(title_font) + 18;
    let max_w = card.width().saturating_sub(64);
    (text_x, subtitle_y, max_w)
}

/// The title + wrapped subtitle every Pairing/Add-host/Wake/Forget-host modal draws
/// before its own content, on top of `draw_modal_card`'s chrome — pulled out once these
/// four had each grown (then separately re-fixed) the same bug: a subtitle positioned a
/// further fixed pixel gap below the title, and drawn as a single unwrapped line, which
/// undersized badly at this app's real TV font scale and let long content run past the
/// card edge. Settings has no subtitle (a divider instead) and doesn't call this. Returns
/// the y just past the wrapped subtitle, for the caller's own content below it.
#[allow(clippy::too_many_arguments)]
pub fn draw_modal_header(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    raster: &dyn TextRaster,
    title_font: FontId,
    subtitle_font: FontId,
    card: Rect,
    title: &str,
    title_color: Color,
    subtitle: &str,
    subtitle_color: Color,
) -> Result<i32> {
    let (text_x, subtitle_y, max_w) = modal_header_geometry(raster, title_font, card);
    draw_text(
        painter,
        text_cache,
        raster,
        title_font,
        title,
        text_x,
        card.y() + 28,
        title_color,
    )?;
    draw_text_wrapped(
        painter,
        text_cache,
        raster,
        subtitle_font,
        subtitle,
        text_x,
        subtitle_y,
        max_w,
        subtitle_color,
        6,
    )
}

/// The same `y` [`draw_modal_header`] would return, computed without drawing —
/// for positioning content below it (e.g. Pairing's PIN row) from `app.rs`'s
/// `prepare_tiles`/`draw_list`, which need that position but must not
/// re-render the header just to get it.
pub fn modal_header_end_y(
    raster: &dyn TextRaster,
    title_font: FontId,
    subtitle_font: FontId,
    card: Rect,
    subtitle: &str,
) -> i32 {
    let (_, subtitle_y, max_w) = modal_header_geometry(raster, title_font, card);
    let lines = wrap_text(raster, subtitle_font, subtitle, max_w).len() as i32;
    subtitle_y + lines * (raster.height(subtitle_font) + 6)
}
