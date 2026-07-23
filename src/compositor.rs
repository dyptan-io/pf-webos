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
use sdl2::rect::Rect;
use sdl2::render::{BlendMode, Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};

use crate::ui::Painter;

/// Identity of one cached tile/texture. `Card` is keyed by grid index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tile {
    /// The focus-free sidebar strip (opaque, screen-height).
    Sidebar,
    /// The currently focused sidebar row (transparent padding + shadow).
    FocusRow,
    /// One grid card, shadow included (transparent padding).
    Card(usize),
    /// The shared focus-ring glow (all cards are the same size).
    Ring,
    /// The active modal, full-screen with transparent surroundings.
    Modal,
    /// The Home status line block (bottom of the grid panel).
    Status,
    /// The "No host selected" hint line.
    NoHost,
    /// The in-stream stats overlay panel (`ui::render_stats_overlay_tile`).
    StatsOverlay,
    /// The in-stream disconnect-confirmation dialog (`ui::render_disconnect_dialog_tile`).
    DisconnectDialog,
}

/// One step of a frame's composition, in paint order.
pub enum DrawCmd {
    /// Copy `tile`'s texture to `dst` (scaled by the GPU if sizes differ),
    /// modulated by `alpha`.
    Tex { tile: Tile, dst: Rect, alpha: u8 },
    /// A blended solid fill — the modal scrim.
    Fill { rect: Rect, color: sdl2::pixels::Color },
}

pub struct Compositor {
    textures: HashMap<Tile, Texture>,
    /// Reused un-premultiply staging buffer (tiny-skia pixmaps are premultiplied;
    /// SDL's `BlendMode::Blend` expects straight alpha — converted once per
    /// *upload*, never per frame).
    staging: Vec<u8>,
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            staging: Vec::new(),
        }
    }

    /// Creates/updates `tile`'s texture from a freshly rasterized painter.
    /// Opaque tiles (`Sidebar`) upload their bytes directly with blending off;
    /// everything else is un-premultiplied into `staging` and alpha-blended.
    pub fn upload(&mut self, creator: &TextureCreator<WindowContext>, tile: Tile, pm: &Painter) -> Result<()> {
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
            self.textures.insert(tile, tex);
        }
        let tex = self.textures.get_mut(&tile).expect("just inserted");
        let pitch = w as usize * 4;
        let opaque = matches!(tile, Tile::Sidebar);
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
                    // premultiplied -> straight: c * 255 / a
                    self.staging.push(((u16::from(px[0]) * 255) / a).min(255) as u8);
                    self.staging.push(((u16::from(px[1]) * 255) / a).min(255) as u8);
                    self.staging.push(((u16::from(px[2]) * 255) / a).min(255) as u8);
                    self.staging.push(px[3]);
                }
            }
            tex.update(None, &self.staging, pitch)
                .map_err(|e| anyhow::anyhow!("upload {tile:?}: {e}"))?;
            tex.set_blend_mode(BlendMode::Blend);
        }
        Ok(())
    }

    /// Executes one frame's draw list. The caller has already cleared the canvas
    /// to the background color.
    pub fn execute(&mut self, canvas: &mut Canvas<Window>, cmds: &[DrawCmd]) -> Result<()> {
        for cmd in cmds {
            match cmd {
                DrawCmd::Tex { tile, dst, alpha } => {
                    let Some(tex) = self.textures.get_mut(tile) else {
                        continue; // not uploaded yet (e.g. art still loading) — skip
                    };
                    tex.set_alpha_mod(*alpha);
                    canvas
                        .copy(tex, None, Some(*dst))
                        .map_err(|e| anyhow::anyhow!("copy {tile:?}: {e}"))?;
                }
                DrawCmd::Fill { rect, color } => {
                    canvas.set_blend_mode(BlendMode::Blend);
                    canvas.set_draw_color(*color);
                    canvas.fill_rect(Some(*rect)).map_err(|e| anyhow::anyhow!("fill: {e}"))?;
                }
            }
        }
        Ok(())
    }
}
