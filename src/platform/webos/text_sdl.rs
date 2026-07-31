//! SDL2_ttf-backed `TextRaster` implementation — the only place in the crate
//! that touches `sdl2::ttf`.
use anyhow::{Context, Result};
use sdl2::ttf::{Font, Sdl2TtfContext};
use tiny_skia::{IntSize, Pixmap};

use crate::ui::render::Color;
use crate::ui::{
    premultiply_rgba, FontId, FontWeight, TextRaster, GEIST_MEDIUM, GEIST_REGULAR, GEIST_SEMIBOLD, ICON_FONT_BYTES,
};

/// Load Geist weight at size proportional to display height (720px reference).
fn load_font(ttf: &Sdl2TtfContext, height_px: u32, design_size: u16, weight: FontWeight) -> Result<Font<'_, 'static>> {
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

/// Loads the bundled icon font at a fixed, generously large size — icon glyphs are
/// always drawn through `draw_icon`, which composites (and, via `Painter`'s
/// bilinear `draw_pixmap_scaled`, downscales) the rasterized glyph to fit whatever
/// rect the caller actually wants, so a single oversized rasterization (rather than
/// one `load_icon_font` call per distinct icon size, the way the three text fonts
/// each get their own) is enough to stay crisp at every icon size this UI uses.
fn load_icon_font(ttf: &Sdl2TtfContext) -> Result<Font<'_, 'static>> {
    let rwops = sdl2::rwops::RWops::from_bytes(ICON_FONT_BYTES).map_err(|e| anyhow::anyhow!("icon font rwops: {e}"))?;
    ttf.load_font_from_rwops(rwops, 128)
        .map_err(|e| anyhow::anyhow!("load_icon_font: {e}"))
}

/// Converts an `SDL2_ttf`-rendered glyph-run surface into an owned, premultiplied
/// `tiny_skia::Pixmap`. Goes through `convert_format(RGBA32)` first so the byte
/// order in memory is always R,G,B,A regardless of `SDL2_ttf`'s actual output format
/// or host endianness — the same `RGBA32` convention `main.rs`/`art.rs` already rely
/// on for raw RGBA buffers.
fn pixmap_from_ttf_surface(surface: &sdl2::surface::Surface) -> Result<Pixmap> {
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

/// Owns the five loaded fonts (sized for a 10-foot TV viewing distance — see
/// `ui`'s `ROW_H`/`ROW_MAX_W` docs) and implements `TextRaster` over them.
pub struct SdlTextRaster<'ttf> {
    label: Font<'ttf, 'static>,
    value: Font<'ttf, 'static>,
    title: Font<'ttf, 'static>,
    icon: Font<'ttf, 'static>,
    caption: Font<'ttf, 'static>,
}

impl<'ttf> SdlTextRaster<'ttf> {
    pub fn new(ttf: &'ttf Sdl2TtfContext, height_px: u32) -> Result<Self> {
        Ok(Self {
            label: load_font(ttf, height_px, 22, FontWeight::Medium)?,
            value: load_font(ttf, height_px, 20, FontWeight::Regular)?,
            title: load_font(ttf, height_px, 40, FontWeight::SemiBold)?,
            icon: load_icon_font(ttf)?,
            caption: load_font(ttf, height_px, 14, FontWeight::Regular)?,
        })
    }

    fn font(&self, id: FontId) -> &Font<'ttf, 'static> {
        match id {
            FontId::Label => &self.label,
            FontId::Value => &self.value,
            FontId::Title => &self.title,
            FontId::Icon => &self.icon,
            FontId::Caption => &self.caption,
        }
    }
}

impl TextRaster for SdlTextRaster<'_> {
    fn rasterize(&self, font: FontId, text: &str, color: Color) -> Result<Pixmap> {
        let surface = self
            .font(font)
            .render(text)
            .blended(sdl2::pixels::Color::RGBA(color.r, color.g, color.b, color.a))
            .map_err(|e| anyhow::anyhow!("render text: {e}"))?;
        pixmap_from_ttf_surface(&surface)
    }

    fn measure(&self, font: FontId, text: &str) -> (u32, u32) {
        self.font(font).size_of(text).unwrap_or((0, 0))
    }

    fn height(&self, font: FontId) -> i32 {
        self.font(font).height()
    }
}
