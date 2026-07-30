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

use crate::app::Screen;
use crate::ui::Painter;

/// Identity of one cached tile/texture. `Card` is keyed by pin id (a
/// `GameEntry::id`, or `store::DESKTOP_PIN_ID`) rather than grid index, so a
/// pin/unpin reorder — which only shuffles positions, not which games exist —
/// never needs to re-upload a card's texture.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Tile {
    /// The focus-free sidebar strip (opaque, screen-height).
    Sidebar,
    /// The currently focused sidebar row (transparent padding + shadow).
    FocusRow,
    /// One grid card, shadow included (transparent padding), keyed by pin id.
    Card(String),
    /// The shared focus-ring glow (all cards are the same size).
    Ring,
    /// The focused card's crisp edge outline, composited on top of the card
    /// art (unlike `Ring`, which sits behind it). Shared, like `Ring`.
    CardOutline,
    /// The pinned badge composited over the focused grid card's top-right
    /// corner, only when that card is pinned. Shared by every card, like `Ring`.
    PinBadge,
    /// The active modal, full-screen with transparent surroundings.
    Modal,
    /// One modal's focused, zoom-animated widget (row, PIN digit, button, etc).
    /// Composited over Modal's shell; one tile covers all modals.
    ModalFocusElement,
    /// Open dropdown panel + option list. Own tile so it composites after `ScrollContent`.
    DropdownOverlay,
    /// Open dropdown's focused option. Composited over `DropdownOverlay`.
    DropdownFocusOption,
    /// The Home status line block (bottom of the grid panel).
    Status,
    /// The "No host selected" hint line.
    NoHost,
    /// Modal scrollbar tile, keyed by Screen (covers all scrollable modals).
    ScrollIndicator(Screen),
    /// Modal scrollable content at unscrolled position. GPU crops/repositions via
    /// `TexCropped`; rebuilds only when content changes, not on scroll.
    ScrollContent(Screen),
    /// Spinner frame texture, keyed by frame index. Held in VRAM until stream starts.
    SpinnerFrame(usize),
    /// The in-stream stats overlay panel (`ui::render_stats_overlay_tile`).
    StatsOverlay,
    /// Transient toast notification (`ui::render_notification_tile`), e.g. the
    /// frame-pacing on/off confirmation after a mid-stream Blue-button toggle.
    Notification,
    /// The log-tail overlay (`ui::render_log_overlay_tile`) — Yellow-button debug
    /// aid, shown on every screen (menu UI and stream), not just while streaming.
    LogOverlay,
    /// Disconnect dialog shell (card + title + unfocused buttons).
    DisconnectDialog,
    /// Disconnect dialog focused button. Composited over `DisconnectDialog` shell.
    DisconnectFocusButton,
}

/// One step of a frame's composition, in paint order.
pub enum DrawCmd {
    /// Copy `tile`'s texture to `dst` (scaled by the GPU if sizes differ),
    /// modulated by `alpha`.
    Tex { tile: Tile, dst: Rect, alpha: u8 },
    /// Copy with source crop — how scrolling works: GPU crops fixed texture.
    TexCropped {
        tile: Tile,
        src: Rect,
        dst: Rect,
        alpha: u8,
    },
    /// A blended solid fill — the modal scrim.
    Fill { rect: Rect, color: sdl2::pixels::Color },
}

pub struct Compositor {
    textures: HashMap<Tile, Texture>,
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
        tile: Tile,
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
            self.textures.insert(tile.clone(), tex);
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
    pub fn drop_tile(&mut self, tile: Tile) {
        if let Some(tex) = self.textures.remove(&tile) {
            // SAFETY: see `clear_all`.
            unsafe { tex.destroy() };
        }
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
                DrawCmd::TexCropped { tile, src, dst, alpha } => {
                    let Some(tex) = self.textures.get_mut(tile) else {
                        continue; // not uploaded yet — skip
                    };
                    tex.set_alpha_mod(*alpha);
                    canvas
                        .copy(tex, Some(*src), Some(*dst))
                        .map_err(|e| anyhow::anyhow!("copy cropped {tile:?}: {e}"))?;
                }
                DrawCmd::Fill { rect, color } => {
                    canvas.set_blend_mode(BlendMode::Blend);
                    canvas.set_draw_color(*color);
                    canvas
                        .fill_rect(Some(*rect))
                        .map_err(|e| anyhow::anyhow!("fill: {e}"))?;
                }
            }
        }
        Ok(())
    }
}
