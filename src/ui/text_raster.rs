//! Text-rasterization seam: `ui` depends only on this trait, never on
//! `sdl2::ttf` directly. `platform::webos::text_sdl` provides the SDL2_ttf-backed
//! implementation.
use crate::ui::render::Color;

/// Opaque handle for one of the app's five loaded fonts (replaces a borrowed
/// `sdl2::ttf::Font` reference).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FontId {
    Label,
    Value,
    Title,
    Icon,
    Caption,
}

/// Rasterizes and measures text. Implemented by `platform::webos::text_sdl::SdlTextRaster`
/// (`SDL2_ttf`) — `ui` never rasterizes glyphs itself.
pub trait TextRaster {
    /// Rasterizes one line to a premultiplied `tiny_skia::Pixmap`.
    fn rasterize(&self, font: FontId, text: &str, color: Color) -> anyhow::Result<tiny_skia::Pixmap>;
    /// Measures `text` in `font` without rasterizing (layout needs this without paying
    /// for a glyph render).
    fn measure(&self, font: FontId, text: &str) -> (u32, u32);
    /// The font's line height (ascent + descent + line gap, per `sdl2::ttf::Font::height`).
    fn height(&self, font: FontId) -> i32;
}
