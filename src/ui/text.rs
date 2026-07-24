//! Font loading, the rasterized-text cache, and text/icon drawing.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use std::collections::HashMap;
use anyhow::{Context, Result};
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::ttf::Font;
use tiny_skia::{IntSize, Pixmap};


/// The bundled Geist family (punktfunk's brand font, the same OTFs every other
/// punktfunk client ships — copied verbatim from `pf-console-ui/assets/fonts/`;
/// license in `assets/fonts/Geist-OFL.txt`). Embedded like the icon font, so
/// nothing needs staging alongside the `.ipk`.
pub static GEIST_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Geist-Regular.otf");
pub static GEIST_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Geist-Medium.otf");
pub static GEIST_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Geist-SemiBold.otf");

/// Which Geist weight to load. (Geist-Bold.otf also sits in `assets/fonts/`,
/// unembedded — add a variant if a Bold use appears; the logo lockup that
/// briefly used it is real artwork now, not text.)
#[derive(Clone, Copy)]
pub enum FontWeight {
    Regular,
    Medium,
    SemiBold,
}

/// The app's four UI fonts, bundled so the many functions needing several of
/// them take one `&Fonts` instead of threading each separately. Borrow-only —
/// the fonts are owned in `main.rs`'s `run_inner` for the whole menu/stream
/// cycle (see `load_font`), so this never needs storing anywhere.
pub struct Fonts<'a, 'ttf> {
    pub label: &'a Font<'ttf, 'static>,
    pub value: &'a Font<'ttf, 'static>,
    pub title: &'a Font<'ttf, 'static>,
    pub icon: &'a Font<'ttf, 'static>,
}

/// Loads a bundled Geist weight at a size proportional to the display height
/// (design reference: a 720px-tall reference screen —
/// `size = design_size * height / 720`).
pub fn load_font(
    ttf: &sdl2::ttf::Sdl2TtfContext,
    height_px: u32,
    design_size: u16,
    weight: FontWeight,
) -> Result<Font<'_, 'static>> {
    let bytes: &'static [u8] = match weight {
        FontWeight::Regular => GEIST_REGULAR,
        FontWeight::Medium => GEIST_MEDIUM,
        FontWeight::SemiBold => GEIST_SEMIBOLD,
    };
    let scaled = (u32::from(design_size) * height_px / 720).max(10) as u16;
    let rwops = sdl2::rwops::RWops::from_bytes(bytes).map_err(|e| anyhow::anyhow!("geist rwops: {e}"))?;
    ttf.load_font_from_rwops(rwops, scaled)
        .map_err(|e| anyhow::anyhow!("load_font (Geist): {e}"))
}

/// The bundled icon font's raw bytes (see the icons section above) — embedded into
/// the binary at compile time, so there's no install-time asset to stage/ship
/// alongside the `.ipk` and no runtime path to resolve.
pub static ICON_FONT_BYTES: &[u8] = include_bytes!("../../assets/icons/MaterialIcons-subset.ttf");

/// The punktfunk logo lockup (mark + FUNK wordmark) — rasterized from the brand's
/// actual vector artwork (`assets/logo/punktfunk-logo-dark.svg`, the dark/no-border
/// variant) at the sidebar's exact display size, so it draws 1:1 with no scaling.
/// See `assets/logo/NOTICE.md` for regeneration.
pub static LOGO_PNG: &[u8] = include_bytes!("../../assets/logo/logo-sidebar.png");

/// Decodes the embedded logo once, lazily (premultiplied, ready to composite).
/// `None` only if the embedded PNG were somehow invalid — the sidebar then just
/// draws without a logo rather than failing.
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

/// Loads the bundled icon font at a fixed, generously large size — icon glyphs are
/// always drawn through `draw_icon`, which composites (and, via `Painter`'s
/// bilinear `draw_pixmap_scaled`, downscales) the rasterized glyph to fit whatever
/// rect the caller actually wants, so a single oversized rasterization (rather than
/// one `load_icon_font` call per distinct icon size, the way the three text fonts
/// each get their own) is enough to stay crisp at every icon size this UI uses.
pub fn load_icon_font(ttf: &sdl2::ttf::Sdl2TtfContext) -> Result<Font<'_, 'static>> {
    let rwops = sdl2::rwops::RWops::from_bytes(ICON_FONT_BYTES).map_err(|e| anyhow::anyhow!("icon font rwops: {e}"))?;
    ttf.load_font_from_rwops(rwops, 128)
        .map_err(|e| anyhow::anyhow!("load_icon_font: {e}"))
}

/// Converts an `SDL2_ttf`-rendered glyph-run surface into an owned, premultiplied
/// `tiny_skia::Pixmap`. Goes through `convert_format(RGBA32)` first so the byte
/// order in memory is always R,G,B,A regardless of `SDL2_ttf`'s actual output format
/// or host endianness — the same `RGBA32` convention `main.rs`/`art.rs` already rely
/// on for raw RGBA buffers.
pub fn pixmap_from_ttf_surface(surface: &sdl2::surface::Surface) -> Result<Pixmap> {
    let surface = surface
        .convert_format(sdl2::pixels::PixelFormatEnum::RGBA32)
        .map_err(|e| anyhow::anyhow!("convert glyph surface: {e}"))?;
    let (w, h) = (surface.width(), surface.height());
    let pitch = surface.pitch() as usize;
    let row_bytes = w as usize * 4;
    let mut rgba = vec![0u8; row_bytes * h as usize];
    surface.with_lock(|src| {
        for y in 0..h as usize {
            let start = y * pitch;
            rgba[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(&src[start..start + row_bytes]);
        }
    });
    premultiply_rgba(&mut rgba);
    Pixmap::from_vec(rgba, IntSize::from_wh(w, h).context("zero-sized glyph surface")?).context("build glyph pixmap")
}

/// Caches rasterized-text `Pixmap`s across frames, keyed by the exact
/// `(text, color, font)` that produced them. Without this, `draw_text` re-rasterized
/// (freetype glyph lookup + blend + premultiply) on *every* call — and every draw
/// function in this module is called on every render tick (the pre-stream UI loop
/// runs at ~60fps), so a static label like "Settings" paid that cost 60 times a
/// second for pixels that never changed. `font` is identified by its address rather
/// than any content: this client only ever loads three fonts once at startup
/// (`font_label`/`font_value`/`font_title` in `main.rs`) and holds them for the
/// whole UI-flow's lifetime, so a stable address is a safe, cheap stand-in for
/// identity — `Font` itself exposes nothing hashable to key on instead. Entry count
/// is naturally bounded by this app's own content (a handful of static labels, a
/// bounded set of settings values, one row per known host/game) — no eviction
/// needed; see module docs if that assumption ever stops holding.
pub struct TextCache {
    entries: HashMap<(String, u32, usize), Pixmap>,
}

impl TextCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    fn key(font: &Font, text: &str, color: Color) -> (String, u32, usize) {
        let packed_color = u32::from_be_bytes([color.r, color.g, color.b, color.a]);
        (text.to_string(), packed_color, std::ptr::from_ref(font) as usize)
    }

    /// Returns the cached `Pixmap` for `(font, text, color)`, rasterizing (and
    /// caching) it first if this is the first time this exact combination has
    /// been drawn.
    fn get_or_create(&mut self, font: &Font, text: &str, color: Color) -> Result<&Pixmap> {
        let key = Self::key(font, text, color);
        if !self.entries.contains_key(&key) {
            let surface = font
                .render(text)
                .blended(color)
                .map_err(|e| anyhow::anyhow!("render text: {e}"))?;
            let pixmap = pixmap_from_ttf_surface(&surface)?;
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
pub fn draw_text(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font: &Font,
    text: &str,
    x: i32,
    y: i32,
    color: Color,
) -> Result<u32> {
    if text.is_empty() {
        return Ok(0);
    }
    let pixmap = text_cache.get_or_create(font, text, color)?;
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
    font: &Font,
    text: &str,
    x: i32,
    y: i32,
    color: Color,
) -> Result<u32> {
    if text.is_empty() {
        return Ok(0);
    }
    let surface = font
        .render(text)
        .blended(color)
        .map_err(|e| anyhow::anyhow!("render text: {e}"))?;
    let pixmap = pixmap_from_ttf_surface(&surface)?;
    let width = pixmap.width();
    painter.draw_pixmap(x, y, &pixmap);
    Ok(width)
}

/// Draws one icon glyph (one of the `ICON_*` constants above) from the bundled icon
/// font, scaled to fill `rect` — the same `TextCache` that caches on-screen text
/// caches these too (a `Font`'s address plus the glyph string is already a unique,
/// stable cache key — see [`TextCache`] — so a second cache wasn't needed just
/// because this one holds icons instead of words).
pub fn draw_icon(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    icon_font: &Font,
    rect: Rect,
    glyph: &str,
    color: Color,
) -> Result<()> {
    let pixmap = text_cache.get_or_create(icon_font, glyph, color)?;
    painter.draw_pixmap_scaled(rect, pixmap);
    Ok(())
}

/// Truncates `text` with a trailing "…" so it fits within `max_w` pixels in `font`
/// (moonlight-tv scroll-marquees long titles on focus instead — see the module docs
/// on why this client keeps it simple).
pub fn ellipsize(font: &Font, text: &str, max_w: u32) -> String {
    if font.size_of(text).map_or(0, |(w, _)| w) <= max_w {
        return text.to_string();
    }
    let mut s: Vec<char> = text.chars().collect();
    while !s.is_empty() {
        s.pop();
        let candidate: String = s.iter().collect::<String>() + "…";
        if font.size_of(&candidate).map_or(0, |(w, _)| w) <= max_w {
            return candidate;
        }
    }
    "…".to_string()
}

/// Greedily word-wraps `text` into lines no wider than `max_w` px in `font` — for modal
/// copy that's a full sentence or two (status/explanation text), unlike `ellipsize`'s
/// single-line truncation for card titles.
pub fn wrap_text(font: &Font, text: &str, max_w: u32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if current.is_empty() || font.size_of(&candidate).map_or(0, |(w, _)| w) <= max_w {
            current = candidate;
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Draws `text` word-wrapped to `max_w` (see [`wrap_text`]), one line per
/// `font.height() + line_gap`, starting at `(x, y)`. Returns the y position just past
/// the last line, so callers can stack more content beneath it without having to guess
/// how many lines it wrapped to.
#[allow(clippy::too_many_arguments)]
pub fn draw_text_wrapped(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font: &Font,
    text: &str,
    x: i32,
    y: i32,
    max_w: u32,
    color: Color,
    line_gap: i32,
) -> Result<i32> {
    let mut cursor_y = y;
    for line in wrap_text(font, text, max_w) {
        draw_text(painter, text_cache, font, &line, x, cursor_y, color)?;
        cursor_y += font.height() + line_gap;
    }
    Ok(cursor_y)
}

/// The pure geometry `draw_modal_header` and `modal_header_end_y` share:
/// `(text_x, subtitle_y, max_w)` — the one place it's computed, so the two
/// can never drift apart.
pub fn modal_header_geometry(title_font: &Font, card: Rect) -> (i32, i32, u32) {
    let text_x = card.x() + 32;
    let title_y = card.y() + 28;
    let subtitle_y = title_y + title_font.height() + 18;
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
    title_font: &Font,
    subtitle_font: &Font,
    card: Rect,
    title: &str,
    title_color: Color,
    subtitle: &str,
    subtitle_color: Color,
) -> Result<i32> {
    let (text_x, subtitle_y, max_w) = modal_header_geometry(title_font, card);
    draw_text(painter, text_cache, title_font, title, text_x, card.y() + 28, title_color)?;
    draw_text_wrapped(painter, text_cache, subtitle_font, subtitle, text_x, subtitle_y, max_w, subtitle_color, 6)
}

/// The same `y` [`draw_modal_header`] would return, computed without drawing —
/// for positioning content below it (e.g. Pairing's PIN row) from `app.rs`'s
/// `prepare_tiles`/`draw_list`, which need that position but must not
/// re-render the header just to get it.
pub fn modal_header_end_y(title_font: &Font, subtitle_font: &Font, card: Rect, subtitle: &str) -> i32 {
    let (_, subtitle_y, max_w) = modal_header_geometry(title_font, card);
    let lines = wrap_text(subtitle_font, subtitle, max_w).len() as i32;
    subtitle_y + lines * (subtitle_font.height() + 6)
}

