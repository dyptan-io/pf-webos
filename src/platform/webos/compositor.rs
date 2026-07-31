//! GPU composition of the pre-stream UI (the `opengles2` SDL renderer confirmed
//! live on-device): tiny-skia rasterizes widgets into cached tiles
//! ([`crate::ui`]'s `render_*_tile` helpers — the AA/soft-shadow look is
//! unchanged), and this module owns their GPU textures and executes `App`'s
//! per-frame draw list. Position, scroll, the focus pop's scale, and fades are
//! all texture-copy parameters here — per-frame CPU rasterization cost is gone,
//! which is what makes 60fps animation feasible on this hardware (the previous
//! CPU compositor measured ~25-45ms/frame; see docs/NOTES.md).
use std::collections::HashMap;

use anyhow::Result;
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::{BlendMode, Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};

use crate::ui::render::{Color, DrawCmd, Rect, TileId};
use crate::ui::Painter;

fn to_sdl_rect(r: Rect) -> sdl2::rect::Rect {
    sdl2::rect::Rect::new(r.x(), r.y(), r.width(), r.height())
}

fn to_sdl_color(c: Color) -> sdl2::pixels::Color {
    sdl2::pixels::Color::RGBA(c.r, c.g, c.b, c.a)
}

pub struct Compositor {
    textures: HashMap<TileId, Texture>,
    /// Reused staging buffer for the premultiplied → straight-alpha conversion
    /// performed once per `upload` call (never per frame).
    staging: Vec<u8>,
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            staging: Vec::new(),
        }
    }

    /// Uploads straight-RGBA8 bytes to a new GPU texture. No-op if already cached.
    pub fn upload_raw(
        &mut self,
        creator: &TextureCreator<WindowContext>,
        tile: TileId,
        w: u32,
        h: u32,
        rgba_straight: &[u8],
    ) -> Result<()> {
        if self.textures.contains_key(&tile) {
            return Ok(());
        }
        let mut tex = creator
            .create_texture_static(PixelFormatEnum::RGBA32, w, h)
            .map_err(|e| anyhow::anyhow!("create texture {tile:?} {w}x{h}: {e}"))?;
        let pitch = w as usize * 4;
        tex.update(None, rgba_straight, pitch)
            .map_err(|e| anyhow::anyhow!("upload {tile:?}: {e}"))?;
        tex.set_blend_mode(BlendMode::Blend);
        self.textures.insert(tile, tex);
        Ok(())
    }

    /// Creates/updates tile's texture from a rasterized painter. Opaque tiles
    /// upload directly; others un-premultiply and alpha-blend.
    pub fn upload(&mut self, creator: &TextureCreator<WindowContext>, tile: TileId, pm: &Painter) -> Result<()> {
        let (w, h) = (pm.width(), pm.height());
        let recreate = match self.textures.get(&tile) {
            Some(t) => {
                let q = t.query();
                q.width != w || q.height != h
            }
            None => true,
        };
        if recreate {
            let tex = creator
                .create_texture_static(PixelFormatEnum::RGBA32, w, h)
                .map_err(|e| anyhow::anyhow!("create texture {tile:?} {w}x{h}: {e}"))?;
            self.textures.insert(tile.clone(), tex);
        }
        let tex = self.textures.get_mut(&tile).expect("just inserted");
        let pitch = w as usize * 4;
        let opaque = matches!(tile, TileId::Sidebar);
        if opaque {
            tex.update(None, pm.data(), pitch)
                .map_err(|e| anyhow::anyhow!("upload {tile:?}: {e}"))?;
            tex.set_blend_mode(BlendMode::None);
        } else {
            let src = pm.data();
            self.staging.clear();
            self.staging.reserve(src.len());
            for px in src.chunks_exact(4) {
                let a = u16::from(px[3]);
                if a == 0 || a == 255 {
                    self.staging.extend_from_slice(px);
                } else {
                    // premultiplied -> straight: c * 255 / a, rounded (not floored) so the
                    // round-trip doesn't bias colours down — see `fill_vertical_fade`.
                    self.staging
                        .push((((u16::from(px[0]) * 255) + a / 2) / a).min(255) as u8);
                    self.staging
                        .push((((u16::from(px[1]) * 255) + a / 2) / a).min(255) as u8);
                    self.staging
                        .push((((u16::from(px[2]) * 255) + a / 2) / a).min(255) as u8);
                    self.staging.push(px[3]);
                }
            }
            tex.update(None, &self.staging, pitch)
                .map_err(|e| anyhow::anyhow!("upload {tile:?}: {e}"))?;
            tex.set_blend_mode(BlendMode::Blend);
        }
        Ok(())
    }

    /// Destroys all cached GPU textures (call on stream start to free VRAM).
    pub fn clear_all(&mut self) {
        // SAFETY: `unsafe_textures` detaches each `Texture` from its creator's
        // lifetime, making the owner responsible for destruction. We drain the
        // map so nothing can reach these textures again, then destroy each one
        // exactly once. Same invariant as `drop_tile`.
        for (_, tex) in self.textures.drain() {
            unsafe { tex.destroy() };
        }
    }

    /// Drops tile's GPU texture. Needed for windowed card tiles to free VRAM
    /// when scrolled out of view (SDL object must be explicitly destroyed).
    pub fn drop_tile(&mut self, tile: TileId) {
        if let Some(tex) = self.textures.remove(&tile) {
            // SAFETY: see `clear_all`.
            unsafe { tex.destroy() };
        }
    }

    /// Executes one frame's draw list. The caller has already cleared the canvas
    /// to the background color.
    pub fn present(&mut self, canvas: &mut Canvas<Window>, cmds: &[DrawCmd]) -> Result<()> {
        for cmd in cmds {
            match cmd {
                DrawCmd::Tex { tile, dst, alpha } => {
                    let Some(tex) = self.textures.get_mut(tile) else {
                        continue; // not uploaded yet (e.g. art still loading) — skip
                    };
                    tex.set_alpha_mod(*alpha);
                    canvas
                        .copy(tex, None, Some(to_sdl_rect(*dst)))
                        .map_err(|e| anyhow::anyhow!("copy {tile:?}: {e}"))?;
                }
                DrawCmd::TexCropped { tile, src, dst, alpha } => {
                    let Some(tex) = self.textures.get_mut(tile) else {
                        continue; // not uploaded yet — skip
                    };
                    tex.set_alpha_mod(*alpha);
                    canvas
                        .copy(tex, Some(to_sdl_rect(*src)), Some(to_sdl_rect(*dst)))
                        .map_err(|e| anyhow::anyhow!("copy cropped {tile:?}: {e}"))?;
                }
                DrawCmd::Fill { rect, color } => {
                    canvas.set_blend_mode(BlendMode::Blend);
                    canvas.set_draw_color(to_sdl_color(*color));
                    canvas
                        .fill_rect(Some(to_sdl_rect(*rect)))
                        .map_err(|e| anyhow::anyhow!("fill: {e}"))?;
                }
            }
        }
        Ok(())
    }
}
