//! Drawing/input-mapping primitives for the pre-stream UI: a persistent sidebar
//! (known hosts + Add host/Settings) beside a detail grid (the selected host's
//! games), plus centered modal cards for Pairing/Settings/Add host — modeled on
//! `mariotaku/moonlight-tv`'s actual layout and dark palette (sidebar + app grid,
//! outline-ring focus, near-square cards).
//!
//! Rendering itself goes through [`Painter`], a thin wrapper around a
//! `tiny_skia::Pixmap` — a pure-Rust software rasterizer giving real anti-aliased
//! fills/strokes and box-blurred shadows (no Skia/Vulkan/LVGL available on webOS;
//! see `docs/NOTES.md`'s "UI" section for why this app doesn't adopt moonlight-tv's
//! actual LVGL toolkit — this UI's whole screen count doesn't warrant a general
//! widget/layout framework, just a better rasterizer than hand-rolled per-scanline
//! SDL2 rects). `main.rs` builds one `Painter` sized to the display, `App::render`
//! draws every screen into it each dirty tick, then `main.rs` uploads the finished
//! buffer to a single SDL2 texture and presents it — one texture/copy per frame,
//! not one per widget.
//!
//! Icons are glyphs from a small bundled, subsetted icon font (see the icons
//! section below and `assets/icons/NOTICE.md`) — the system font covers ASCII
//! only (see `SYSTEM_FONT_PATH`'s docs), so real icon glyphs need one of their own.
use std::collections::HashMap;

use anyhow::{Context, Result};
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::ttf::Font;
use tiny_skia::{
    Color as SkColor, FillRule, FilterQuality, IntSize, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};

use crate::discovery::DiscoveredHost;
use crate::store::{KnownHost, Settings};

// ---------------------------------------------------------------------- palette --
// moonlight-tv has no dedicated colors header — these are the literals its views
// use at each call site (theme file only wires fonts/layout, not color constants).

pub const BG: Color = Color::RGB(0x1e, 0x1e, 0x1e);
pub const SIDEBAR_BG: Color = Color::RGB(0x2a, 0x2d, 0x31);
pub const ACCENT: Color = Color::RGB(0x21, 0x96, 0xf3);
pub const ACCENT_BRIGHT: Color = Color::RGB(0x5f, 0xb4, 0xf7);
pub const WARNING: Color = Color::RGB(0xff, 0xc1, 0x07);
pub const ERROR_RED: Color = Color::RGB(0xff, 0x6b, 0x6b);
pub const WHITE: Color = Color::RGB(0xf5, 0xf5, 0xf5);
pub const MUTED: Color = Color::RGB(0x9e, 0x9e, 0x9e);
pub const MODAL_SCRIM: Color = Color::RGBA(0x00, 0x00, 0x00, 0x80);

/// LG's own system UI font — already on-device, no bundling needed (see Cargo.toml).
pub const SYSTEM_FONT_PATH: &str = "/usr/share/fonts/LG_Smart_UI-Regular.ttf";

// ------------------------------------------------------------------------ icons --
// Every icon in this UI is a glyph from a bundled, subsetted copy of Google's
// Material Icons font (`assets/icons/MaterialIcons-subset.ttf`, Apache 2.0 — see
// `assets/icons/NOTICE.md` for provenance/license and how to regenerate the subset)
// rather than a vector-drawn shape: the system font covers ASCII only (see
// `load_font`'s docs), so real icon glyphs need a font of their own, and a real
// icon font draws a cleaner tv/lock/gear/etc. than hand-rolled path math ever did.
// Rendered the same way as any other text (`draw_icon` reuses `TextCache`/`Font`),
// just scaled to fit the icon's rect afterward — see `draw_icon`.

pub const ICON_TV: &str = "\u{E333}";
pub const ICON_LOCK: &str = "\u{E897}";
pub const ICON_ADD: &str = "\u{E145}";
pub const ICON_CLOSE: &str = "\u{E5CD}";
pub const ICON_SETTINGS: &str = "\u{E8B8}";
pub const ICON_MONITOR: &str = "\u{EF5B}";
pub const ICON_SCHEDULE: &str = "\u{E8B5}";
pub const ICON_SIGNAL: &str = "\u{E202}";
pub const ICON_SUN: &str = "\u{E430}";
pub const ICON_CHEVRON_DOWN: &str = "\u{E5C5}";
pub const ICON_POWER: &str = "\u{E8AC}";
pub const ICON_DELETE: &str = "\u{E872}";

// -------------------------------------------------------------------- input map --

/// A menu event, already debounced from the raw SDL2 input (keyboard arrows — which
/// the webOS Magic Remote's d-pad mode surfaces as — and gamepad d-pad both map here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    Up,
    Down,
    Left,
    Right,
    Confirm,
    Back,
    /// "Forget this host" on the sidebar — deliberately a separate key from
    /// Back/Confirm so it can't be hit by accident (see `app.rs`).
    Secondary,
}

pub fn menu_event_for_key(keycode: sdl2::keyboard::Keycode) -> Option<MenuEvent> {
    use sdl2::keyboard::Keycode;
    Some(match keycode {
        Keycode::Up => MenuEvent::Up,
        Keycode::Down => MenuEvent::Down,
        Keycode::Left => MenuEvent::Left,
        Keycode::Right => MenuEvent::Right,
        Keycode::Return | Keycode::Return2 | Keycode::KpEnter => MenuEvent::Confirm,
        // AcBack: some remotes' dedicated Back button sends the browser-style "AC
        // Back" key rather than Escape/Backspace — map all three so Back works
        // regardless of which one this remote actually sends.
        Keycode::Backspace | Keycode::Escape | Keycode::AcBack => MenuEvent::Back,
        Keycode::Delete => MenuEvent::Secondary,
        _ => return None,
    })
}

pub fn menu_event_for_button(button: sdl2::controller::Button) -> Option<MenuEvent> {
    use sdl2::controller::Button;
    Some(match button {
        Button::DPadUp => MenuEvent::Up,
        Button::DPadDown => MenuEvent::Down,
        Button::DPadLeft => MenuEvent::Left,
        Button::DPadRight => MenuEvent::Right,
        Button::A => MenuEvent::Confirm,
        Button::B => MenuEvent::Back,
        Button::Y => MenuEvent::Secondary,
        _ => return None,
    })
}

/// Left-stick tilt past this fraction of full deflection (of i16's ±32768) counts as
/// a directional press — well past center-rest noise.
const STICK_MENU_DEADZONE: i16 = 16_000;

/// Edge-detects the left stick's X/Y axes into `MenuEvent`s, per-axis, so a hold
/// fires once on crossing the deadzone and doesn't repeat until the stick passes back
/// through center — the same one-shot-per-press behavior a D-pad button already has
/// (SDL2 doesn't auto-repeat `ControllerButtonDown` while held).
#[derive(Default)]
pub struct StickMenuNav {
    x: Option<MenuEvent>,
    y: Option<MenuEvent>,
}

impl StickMenuNav {
    pub fn axis_event(&mut self, axis: sdl2::controller::Axis, value: i16) -> Option<MenuEvent> {
        use sdl2::controller::Axis;
        match axis {
            Axis::LeftX => Self::edge(&mut self.x, value, MenuEvent::Left, MenuEvent::Right),
            // Positive Y is up on SDL2's GameController axis (see `gamepad.rs`'s
            // `axis_event` docs), so a positive tilt maps to `Up`, not `Down`.
            Axis::LeftY => Self::edge(&mut self.y, value, MenuEvent::Down, MenuEvent::Up),
            _ => None,
        }
    }

    fn edge(state: &mut Option<MenuEvent>, value: i16, neg: MenuEvent, pos: MenuEvent) -> Option<MenuEvent> {
        let dir = if value <= -STICK_MENU_DEADZONE {
            Some(neg)
        } else if value >= STICK_MENU_DEADZONE {
            Some(pos)
        } else {
            None
        };
        if dir == *state {
            return None;
        }
        *state = dir;
        dir
    }
}

/// The Magic Remote's number buttons (0-9) surface as plain keyboard digit keys —
/// used for direct PIN entry (type a digit, auto-advance) instead of cycling each
/// digit with left/right.
pub fn digit_key_value(keycode: sdl2::keyboard::Keycode) -> Option<u8> {
    use sdl2::keyboard::Keycode;
    Some(match keycode {
        Keycode::Num0 | Keycode::Kp0 => 0,
        Keycode::Num1 | Keycode::Kp1 => 1,
        Keycode::Num2 | Keycode::Kp2 => 2,
        Keycode::Num3 | Keycode::Kp3 => 3,
        Keycode::Num4 | Keycode::Kp4 => 4,
        Keycode::Num5 | Keycode::Kp5 => 5,
        Keycode::Num6 | Keycode::Kp6 => 6,
        Keycode::Num7 | Keycode::Kp7 => 7,
        Keycode::Num8 | Keycode::Kp8 => 8,
        Keycode::Num9 | Keycode::Kp9 => 9,
        _ => return None,
    })
}

// --------------------------------------------------------------------- painter --
// The AA rendering backend: a `tiny_skia::Pixmap` framebuffer plus the handful of
// primitive ops every higher-level `draw_*` function below is built from. Nothing
// past this section touches SDL2 rendering at all — `Font`/`Surface` still come
// from `SDL2_ttf` (text metrics/rasterization; see the text/font section), but the
// actual pixels always end up composited into this same buffer.

fn sk_color(c: Color) -> SkColor {
    SkColor::from_rgba8(c.r, c.g, c.b, c.a)
}

/// A flat-color `Paint` — every fill/stroke in this module uses one of these and
/// nothing fancier (no gradients/patterns needed for this UI).
///
/// Anti-aliasing off: tiny-skia dispatches to a genuinely separate, cheaper
/// scan-conversion path when `anti_alias` is off (`scan::path::fill_path`/
/// `scan::fill_rect` vs. the `_aa` variants) — measured on real webOS hardware,
/// worth a real (if modest, ~15-25%) chunk of render time on larger fills like the
/// Settings modal card. See docs/NOTES.md's "UI performance, round 2" entry.
fn solid_paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(sk_color(color));
    paint.anti_alias = false;
    paint
}

/// Builds a rounded-rect as a Bezier path — tiny-skia (unlike full Skia) has no
/// built-in rounded-rect primitive. `k` is the standard cubic-Bezier
/// circular-arc-approximation constant. Falls back to a plain rect once `radius`
/// clamps to ~0.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<tiny_skia::Path> {
    /// The standard cubic-Bezier circular-arc-approximation constant.
    const K: f32 = 0.552_284_7;

    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    if r < 0.5 {
        pb.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
        return pb.finish();
    }
    let k = K * r;
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// One frame's whole-screen framebuffer. `App::render` draws every screen into a
/// single `Painter`; `main.rs` uploads the result to one SDL2 texture and presents
/// it, rather than issuing a texture copy per widget as the old canvas-based
/// version did.
pub struct Painter {
    pixmap: Pixmap,
    /// Rendered (padded, box-blurred) shadow shapes, keyed by the params that
    /// fully determine their pixels — every card of a given size/style shares
    /// one entry instead of re-running the blur every dirty frame. Small and
    /// bounded: the UI only ever draws a handful of distinct card sizes
    /// (poster cards, sidebar rows, modals).
    shadow_cache: HashMap<ShadowKey, Pixmap>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ShadowKey {
    w: u32,
    h: u32,
    radius: i32,
    blur_bits: u32,
    opacity: u8,
}

impl Painter {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pixmap: Pixmap::new(width.max(1), height.max(1)).expect("nonzero framebuffer size"),
            shadow_cache: HashMap::new(),
        }
    }

    /// Raw premultiplied RGBA8 bytes, row-major, `width() * height() * 4` long —
    /// the exact byte order `sdl2::pixels::PixelFormatEnum::RGBA32` expects, so
    /// `main.rs` can upload it to an SDL2 texture with no further conversion (every
    /// frame starts with an opaque `clear`, so alpha is 255 everywhere by the time
    /// this is read — premultiplied and straight are then identical).
    pub fn data(&self) -> &[u8] {
        self.pixmap.data()
    }

    /// Fills the whole frame — always the first call of a frame, matching the old
    /// canvas's `clear()` (this UI has no transparent regions of its own; whatever
    /// isn't covered by a widget just shows this color).
    pub fn clear(&mut self, color: Color) {
        self.pixmap.fill(sk_color(color));
    }

    /// Darkens the whole framebuffer by blending in flat black at `alpha` —
    /// `draw_modal_backdrop`'s scrim behind every modal (Settings/Pairing/AddHost/
    /// Wake). Used to be a plain `fill_rect` over the whole 1920x1080 frame with a
    /// semi-transparent color, going through tiny-skia's general shader/blend
    /// pipeline — measured on real webOS hardware, that one full-screen blend
    /// (~2M pixels) was the dominant per-frame cost on every modal screen (a
    /// "render split" log showed the modal's *entire* extra cost over Home,
    /// ~300ms+, unaffected by anything else on screen). Since the source color is
    /// always plain black, `SourceOver` onto a fully-opaque destination reduces to
    /// `dst *= (255 - alpha) / 255` — implemented directly here as a raw pixel
    /// loop (no shader/pipeline construction, no floats) instead of going through
    /// `fill_rect`.
    pub fn dim(&mut self, alpha: u8) {
        let keep = u32::from(255 - alpha);
        for px in self.pixmap.data_mut().chunks_exact_mut(4) {
            px[0] = ((u32::from(px[0]) * keep) / 255) as u8;
            px[1] = ((u32::from(px[1]) * keep) / 255) as u8;
            px[2] = ((u32::from(px[2]) * keep) / 255) as u8;
        }
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.fill_rounded_rect(rect, 0, color);
    }

    pub fn fill_rounded_rect(&mut self, rect: Rect, radius: i32, color: Color) {
        let (w, h) = (rect.width() as f32, rect.height() as f32);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let Some(path) = rounded_rect_path(rect.x() as f32, rect.y() as f32, w, h, radius as f32) else {
            return;
        };
        self.fill(&path, color);
    }

    pub fn stroke_rounded_rect(&mut self, rect: Rect, radius: i32, color: Color, width: f32) {
        let (w, h) = (rect.width() as f32, rect.height() as f32);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let Some(path) = rounded_rect_path(rect.x() as f32, rect.y() as f32, w, h, radius as f32) else {
            return;
        };
        let paint = solid_paint(color);
        let stroke = Stroke {
            width,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, color: Color) {
        if r <= 0.0 {
            return;
        }
        let Some(path) = PathBuilder::from_circle(cx, cy, r) else {
            return;
        };
        self.fill(&path, color);
    }

    fn fill(&mut self, path: &tiny_skia::Path, color: Color) {
        let paint = solid_paint(color);
        self.pixmap
            .fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    /// A soft, real (box-blurred) drop shadow for a rounded-rect shape, offset by
    /// `(dx, dy)` — replaces the old flat single-offset hard-edged rect, which had
    /// no actual softness to sell "shadow" at TV viewing distance.
    ///
    /// The blurred shape only depends on `(rect.width(), rect.height(), radius,
    /// blur, opacity)`, not position — every card of the same size/style (the
    /// whole game grid, every sidebar row) reuses one cached shape instead of
    /// re-running the box blur per card per frame.
    pub fn fill_shadow(&mut self, rect: Rect, radius: i32, dx: f32, dy: f32, blur: f32, opacity: u8) {
        if rect.width() == 0 || rect.height() == 0 {
            return;
        }
        let pad = blur.ceil().max(0.0) as i32 + 1;
        let key = ShadowKey {
            w: rect.width(),
            h: rect.height(),
            radius,
            blur_bits: blur.to_bits(),
            opacity,
        };
        let shape = match self.shadow_cache.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let Some(shape) = render_shadow_shape(rect.width(), rect.height(), radius, pad, blur, opacity) else {
                    return;
                };
                e.insert(shape)
            }
        };
        self.pixmap.draw_pixmap(
            rect.x() - pad + dx.round() as i32,
            rect.y() - pad + dy.round() as i32,
            shape.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }

    pub fn draw_pixmap(&mut self, x: i32, y: i32, src: &Pixmap) {
        self.pixmap
            .draw_pixmap(x, y, src.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }

    /// Stamps `other`'s whole framebuffer over this one — used to composite a
    /// separately cached, less-frequently-rerendered layer (see `App::render`'s
    /// `home_layer`) as this frame's backdrop. A raw buffer copy, not
    /// `draw_pixmap`/`fill_rect`: measured on real webOS hardware, routing a
    /// full-frame composite through tiny-skia's general shader/blend pipeline
    /// (same root cause as `dim`'s docs) cost ~330-350ms on its own — *more* than
    /// the `render_home` call this cache exists to avoid, making the cache a net
    /// loss until this changed. Both `Painter`s are always the same screen size
    /// in practice (both sized from the same display mode), so a straight
    /// `copy_from_slice` is always valid here.
    pub fn blit_layer(&mut self, other: &Self) {
        self.pixmap.data_mut().copy_from_slice(other.pixmap.data());
    }

    /// Composites `src` scaled to exactly fill `dst` — `image`-decoded cover art
    /// (see `art.rs`) is already downscaled close to display size, so this is just
    /// a small final-fit correction, not doing the heavy lifting of the downscale.
    /// `FilterQuality::Nearest`, not `Bilinear`: tiny-skia's bilinear `Pattern`
    /// pushes extra interpolation stages into its raster pipeline whenever the
    /// transform scales (see `Pattern::push_stages`), which measured as a real
    /// (if modest) per-call cost on real webOS hardware — see docs/NOTES.md's "UI
    /// performance, round 2" entry.
    pub fn draw_pixmap_scaled(&mut self, dst: Rect, src: &Pixmap) {
        let (dw, dh) = (dst.width() as f32, dst.height() as f32);
        let (sw, sh) = (src.width() as f32, src.height() as f32);
        if dw <= 0.0 || dh <= 0.0 || sw <= 0.0 || sh <= 0.0 {
            return;
        }
        let transform = Transform::from_scale(dw / sw, dh / sh).post_translate(dst.x() as f32, dst.y() as f32);
        let paint = PixmapPaint {
            quality: FilterQuality::Nearest,
            ..PixmapPaint::default()
        };
        self.pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    }
}

/// How far a shadow's blur extends past the shape casting it, in px — a fixed
/// constant (not derived from anything) picked to read as a soft TV-scale shadow.
const SHADOW_BLUR: f32 = 14.0;

/// Rasterizes a `(w, h)` rounded-rect shape into a small padded alpha buffer and
/// box-blurs it (3 passes — a cheap approximation of a Gaussian blur, good enough
/// at TV viewing distance for a drop shadow), returning the standalone shadow
/// shape as a black, premultiplied `Pixmap` ready to be composited at any
/// position — see `Painter::fill_shadow`'s cache, keyed on everything that
/// determines these pixels (size/radius/blur/opacity, not position).
fn render_shadow_shape(w: u32, h: u32, radius: i32, pad: i32, blur: f32, opacity: u8) -> Option<Pixmap> {
    let (pw, ph) = (w as i32 + 2 * pad, h as i32 + 2 * pad);
    if pw <= 0 || ph <= 0 {
        return None;
    }
    let mut shape = Pixmap::new(pw as u32, ph as u32)?;
    let path = rounded_rect_path(pad as f32, pad as f32, w as f32, h as f32, radius as f32)?;
    let paint = solid_paint(Color::RGBA(0, 0, 0, opacity));
    shape.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);

    // tiny-skia stores premultiplied RGBA; a pure-black shape's R/G/B channels are
    // always 0, so its alpha channel alone fully describes the shape — blur that
    // channel directly rather than blurring all 4 for no visual difference.
    let mut alpha: Vec<u8> = shape.data().iter().skip(3).step_by(4).copied().collect();
    let radius_px = (blur / 2.0).round().max(1.0) as usize;
    for _ in 0..3 {
        box_blur(&mut alpha, pw as usize, ph as usize, radius_px);
    }
    for (i, a) in alpha.into_iter().enumerate() {
        shape.data_mut()[i * 4 + 3] = a; // R/G/B stay 0 (premultiplied black)
    }

    Some(shape)
}

/// Separable box blur (horizontal pass into `tmp`, then vertical back into
/// `pixels`) — both passes are the same 1D sliding-window average, just walking
/// the buffer in a different direction (see `blur_1d`).
fn box_blur(pixels: &mut [u8], w: usize, h: usize, radius: usize) {
    if radius == 0 {
        return;
    }
    let mut tmp = vec![0u8; pixels.len()];
    for y in 0..h {
        blur_1d(w, radius, |x| pixels[y * w + x], |x, v| tmp[y * w + x] = v);
    }
    for x in 0..w {
        blur_1d(h, radius, |y| tmp[y * w + x], |y, v| pixels[y * w + x] = v);
    }
}

/// A 1D sliding-window average over `len` samples (read/written through the given
/// accessors, so the same core serves both a blur's horizontal and vertical
/// passes), via a prefix sum so each output sample is O(1) regardless of `radius`.
fn blur_1d(len: usize, radius: usize, read: impl Fn(usize) -> u8, mut write: impl FnMut(usize, u8)) {
    let mut prefix = vec![0u32; len + 1];
    for i in 0..len {
        prefix[i + 1] = prefix[i] + u32::from(read(i));
    }
    for i in 0..len {
        let lo = i.saturating_sub(radius);
        let hi = (i + radius).min(len - 1);
        let count = (hi - lo + 1) as u32;
        write(i, ((prefix[hi + 1] - prefix[lo]) / count) as u8);
    }
}

/// `tiny-skia` stores premultiplied alpha; `SDL2_ttf`'s `.blended()` glyph surfaces
/// and `image`'s decoded covers are both straight alpha — every raw-RGBA buffer
/// feeding a `Pixmap` (see `pixmap_from_ttf_surface`, `art.rs`) goes through this
/// first.
pub fn premultiply_rgba(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        px[0] = ((u32::from(px[0]) * a) / 255) as u8;
        px[1] = ((u32::from(px[1]) * a) / 255) as u8;
        px[2] = ((u32::from(px[2]) * a) / 255) as u8;
    }
}

// --------------------------------------------------------------------- text/font --

/// Loads the system font at a size proportional to the display height (design
/// reference: a 720px-tall reference screen — `size = design_size * height / 720`).
pub fn load_font<'a>(
    ttf: &'a sdl2::ttf::Sdl2TtfContext,
    height_px: u32,
    design_size: u16,
) -> Result<Font<'a, 'static>> {
    let scaled = (u32::from(design_size) * height_px / 720).max(10) as u16;
    ttf.load_font(SYSTEM_FONT_PATH, scaled)
        .map_err(|e| anyhow::anyhow!("load_font {SYSTEM_FONT_PATH}: {e}"))
}

/// The bundled icon font's raw bytes (see the icons section above) — embedded into
/// the binary at compile time, so there's no install-time asset to stage/ship
/// alongside the `.ipk` and no runtime path to resolve.
static ICON_FONT_BYTES: &[u8] = include_bytes!("../assets/icons/MaterialIcons-subset.ttf");

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

// -------------------------------------------------------------------- focus/cards --

/// A slight softening of moonlight-tv's near-square (~2px) tile radius.
pub const CARD_RADIUS: i32 = 10;
pub const MODAL_RADIUS: i32 = 20;

/// Approximates moonlight-tv's 102%/99% focus/press zoom (a real `transform_zoom`
/// isn't worth a transform pipeline here) by inflating the drawn rect a few percent
/// from its own center when focused.
fn inflate(rect: Rect, focused: bool) -> Rect {
    if !focused {
        return rect;
    }
    let grow_w = ((rect.width() as f32) * 0.02).round() as i32;
    let grow_h = ((rect.height() as f32) * 0.02).round() as i32;
    Rect::new(
        rect.x() - grow_w,
        rect.y() - grow_h,
        rect.width() + 2 * grow_w as u32,
        rect.height() + 2 * grow_h as u32,
    )
}

/// A soft, real drop shadow (see [`Painter::fill_shadow`]) — matches the reference's
/// shadowed-card look.
fn draw_card_shadow(painter: &mut Painter, rect: Rect, radius: i32) {
    painter.fill_shadow(rect, radius, 3.0, 5.0, SHADOW_BLUR, 0x60);
}

/// moonlight-tv's focus cue is an outline ring offset outward from the tile, not a
/// filled/background change — bright accent blue, invisible unless focused. Two
/// passes at increasing offset/decreasing alpha approximate a soft glow. Only
/// `draw_poster_card` (game/Desktop grid selection) uses this — every other
/// selectable row/button relies on [`draw_selectable`]'s zoom, focus-only card,
/// and text-color change instead, per an explicit request to drop rings
/// everywhere except game selection.
fn draw_focus_ring(painter: &mut Painter, rect: Rect, radius: i32) {
    let passes = [(3, 0xff), (6, 0x60)];
    for (offset, alpha) in passes {
        let ring = Rect::new(
            rect.x() - offset,
            rect.y() - offset,
            rect.width() + 2 * offset as u32,
            rect.height() + 2 * offset as u32,
        );
        let color = Color::RGBA(ACCENT_BRIGHT.r, ACCENT_BRIGHT.g, ACCENT_BRIGHT.b, alpha);
        painter.stroke_rounded_rect(ring, radius + offset, color, 2.0);
    }
}

/// Draws a plain surface card for a text-entry field (PIN/IP digit boxes) — always
/// visible, so every slot reads as "a box you can fill in", not just the current
/// one — shadow and `SIDEBAR_BG` fill, zoom-inflated slightly when focused. Returns
/// the (possibly zoom-inflated) rect actually drawn, so callers can center content
/// inside it. Selectable rows/buttons use [`draw_selectable`] instead, which only
/// paints the box when focused.
pub fn draw_card(painter: &mut Painter, rect: Rect, focused: bool) -> Rect {
    let r = inflate(rect, focused);
    draw_card_shadow(painter, r, CARD_RADIUS);
    painter.fill_rounded_rect(r, CARD_RADIUS, SIDEBAR_BG);
    r
}

/// Same card as [`draw_card`], but only painted when focused — an unfocused
/// row/button has no background at all. Used by every selectable row/button
/// (sidebar, settings, Wake, confirm).
fn draw_selectable(painter: &mut Painter, rect: Rect, focused: bool) -> Rect {
    let r = inflate(rect, focused);
    if focused {
        draw_card_shadow(painter, r, CARD_RADIUS);
        painter.fill_rounded_rect(r, CARD_RADIUS, SIDEBAR_BG);
    }
    r
}

/// A handful of muted hues for the poster-card placeholder tint (hash-selected per
/// title, not arbitrary RGB) — kept dark enough that white text stays legible.
const POSTER_TINTS: [Color; 6] = [
    Color::RGB(0x5b, 0x3a, 0x8e), // violet
    Color::RGB(0x1f, 0x6f, 0x8c), // teal
    Color::RGB(0x8c, 0x3a, 0x4a), // maroon
    Color::RGB(0x3a, 0x6f, 0x3a), // green
    Color::RGB(0x8c, 0x6a, 0x1f), // amber-brown
    Color::RGB(0x3a, 0x4a, 0x8c), // indigo
];

fn tint_for(title: &str) -> Color {
    let hash = title
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(u32::from(b)));
    POSTER_TINTS[hash as usize % POSTER_TINTS.len()]
}

/// Draws one game/Desktop tile. `art`, when `Some` (a decoded cover, already
/// downscaled and premultiplied by `art.rs`), fills the whole card, same as
/// moonlight-tv's cover-image tiles; `None` falls back to a tinted placeholder +
/// initial letter (no real art fetched yet, or the host has none for this title).
/// Either way a bottom title strip overlays the art/tint, matching the reference's
/// always-present (ellipsized) title label.
#[allow(clippy::too_many_arguments)]
pub fn draw_poster_card(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_title: &Font,
    font_value: &Font,
    rect: Rect,
    title: &str,
    art: Option<&Pixmap>,
    focused: bool,
) -> Result<()> {
    let r = inflate(rect, focused);
    draw_card_shadow(painter, r, CARD_RADIUS);

    let strip_h = (font_value.height() + 16).min(r.height() as i32 / 3);
    match art {
        Some(pixmap) => {
            painter.draw_pixmap_scaled(r, pixmap);
        }
        None => {
            painter.fill_rounded_rect(r, CARD_RADIUS, tint_for(title));
            let initial = title
                .chars()
                .find(|c| c.is_alphanumeric())
                .unwrap_or('?')
                .to_uppercase()
                .to_string();
            let (iw, ih) = font_title.size_of(&initial).unwrap_or((0, 0));
            let art_h = r.height() as i32 - strip_h;
            draw_text(
                painter,
                text_cache,
                font_title,
                &initial,
                r.x() + (r.width() as i32 - iw as i32) / 2,
                r.y() + (art_h - ih as i32) / 2,
                Color::RGBA(0xff, 0xff, 0xff, 0xa0),
            )?;
        }
    }

    let strip = Rect::new(
        r.x() + 2,
        r.y() + r.height() as i32 - strip_h,
        r.width().saturating_sub(4),
        strip_h.max(0) as u32,
    );
    painter.fill_rect(strip, Color::RGBA(0x00, 0x00, 0x00, 0x70));
    let label = ellipsize(font_value, title, strip.width().saturating_sub(16));
    draw_text(
        painter,
        text_cache,
        font_value,
        &label,
        strip.x() + 8,
        strip.y() + (strip.height() as i32 - font_value.height()) / 2,
        WHITE,
    )?;

    if focused {
        draw_focus_ring(painter, r, CARD_RADIUS);
    }
    Ok(())
}

// ---------------------------------------------------------------------- sidebar --

// Sized for a 10-foot TV viewing distance, not a desktop/phone screen.
pub const SIDEBAR_W: u32 = 400;
pub const SIDEBAR_PAD: i32 = 24;
pub const SIDEBAR_TOP_Y: i32 = 160;
pub const SIDEBAR_ROW_H: u32 = 76;
pub const SIDEBAR_ROW_GAP: i32 = 10;

pub fn sidebar_row_rect(index: usize) -> Rect {
    let y = SIDEBAR_TOP_Y + index as i32 * (SIDEBAR_ROW_H as i32 + SIDEBAR_ROW_GAP);
    Rect::new(SIDEBAR_PAD, y, SIDEBAR_W - 2 * SIDEBAR_PAD as u32, SIDEBAR_ROW_H)
}

/// The "Settings" row's rect — pinned to the bottom of the sidebar panel instead
/// of following the host list/"+ Add host" row sequentially (`sidebar_row_rect`),
/// so it stays in the same place regardless of how many hosts are known.
pub fn settings_row_rect(screen_h: u32) -> Rect {
    let y = screen_h as i32 - SIDEBAR_PAD - SIDEBAR_ROW_H as i32;
    Rect::new(SIDEBAR_PAD, y, SIDEBAR_W - 2 * SIDEBAR_PAD as u32, SIDEBAR_ROW_H)
}

/// `None` when `(x, y)` falls outside the sidebar's horizontal band at all — lets
/// mouse-motion handling distinguish "not hovering the sidebar" from "hovering the
/// sidebar but between rows." The last nav position (`row_count - 1`, "Settings")
/// is pinned to the bottom of the panel (see `settings_row_rect`) rather than
/// following on from the sequential rows above it.
pub fn hit_test_sidebar_row(x: i32, y: i32, row_count: usize, screen_h: u32) -> Option<usize> {
    if x < 0 || x as u32 > SIDEBAR_W || row_count == 0 {
        return None;
    }
    let settings_index = row_count - 1;
    if settings_row_rect(screen_h).contains_point((x, y)) {
        return Some(settings_index);
    }
    (0..settings_index).find(|&i| sidebar_row_rect(i).contains_point((x, y)))
}

/// One entry in the sidebar's host list — either a fully known/paired host or a
/// freshly discovered (not yet paired) one.
#[derive(Clone)]
pub enum HostEntry {
    Known(KnownHost),
    Discovered(DiscoveredHost),
}

impl HostEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Known(h) => &h.name,
            Self::Discovered(h) => &h.name,
        }
    }
    pub fn host(&self) -> &str {
        match self {
            Self::Known(h) => &h.host,
            Self::Discovered(h) => &h.addr,
        }
    }
    pub fn port(&self) -> u16 {
        match self {
            Self::Known(h) => h.port,
            Self::Discovered(h) => h.port,
        }
    }
    pub fn is_paired(&self) -> bool {
        matches!(self, Self::Known(h) if h.fingerprint.is_some())
    }
    pub fn mgmt_port(&self) -> Option<u16> {
        match self {
            Self::Known(h) => h.mgmt_port,
            Self::Discovered(h) => h.mgmt_port,
        }
    }
    /// Wake-on-LAN MAC(s) known for this entry so far — empty until it's been seen
    /// advertising its `mac` mDNS TXT at least once (see `discovery::DiscoveredHost::mac`).
    pub fn mac(&self) -> &[String] {
        match self {
            Self::Known(h) => &h.mac,
            Self::Discovered(h) => &h.mac,
        }
    }
}

/// Draws the whole sidebar: a flat `SIDEBAR_BG` panel, a "punktfunk" wordmark at
/// the top, one row per host (icon reflects paired/not-paired), a trailing
/// "+ Add host" row, and "Settings" pinned to the very bottom of the panel (see
/// `settings_row_rect`) rather than following on from the host list — it stays
/// put regardless of how many hosts are known, instead of drifting down the
/// screen as the list grows. `focused_index` is `Some` only when the sidebar
/// itself has focus (see `app.rs`'s `HomeFocus`).
#[allow(clippy::too_many_arguments)]
pub fn draw_sidebar(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    font_title: &Font,
    icon_font: &Font,
    entries: &[HostEntry],
    focused_index: Option<usize>,
    screen_h: u32,
) -> Result<()> {
    painter.fill_rect(Rect::new(0, 0, SIDEBAR_W, screen_h), SIDEBAR_BG);
    draw_text(painter, text_cache, font_title, "punktfunk", SIDEBAR_PAD, 56, WHITE)?;

    let add_row = entries.len();
    let settings_row = entries.len() + 1;
    for (i, entry) in entries.iter().enumerate() {
        draw_host_row(
            painter,
            text_cache,
            font_label,
            icon_font,
            sidebar_row_rect(i),
            entry.name(),
            entry.is_paired(),
            focused_index == Some(i),
        )?;
    }
    draw_utility_row(
        painter,
        text_cache,
        font_label,
        icon_font,
        sidebar_row_rect(add_row),
        "+ Add host",
        focused_index == Some(add_row),
    )?;

    let settings_rect = settings_row_rect(screen_h);
    painter.fill_rect(
        Rect::new(settings_rect.x(), settings_rect.y() - 14, settings_rect.width(), 1),
        Color::RGBA(0xff, 0xff, 0xff, 0x1a),
    );
    draw_utility_row(
        painter,
        text_cache,
        font_label,
        icon_font,
        settings_rect,
        "Settings",
        focused_index == Some(settings_row),
    )?;

    Ok(())
}

/// Shared layout for every sidebar row (host rows and the "+ Add host"/
/// "Settings" utility rows alike): a left-aligned icon and a label, both
/// colored by focus, plus the [`draw_selectable`] card that only appears
/// (zoomed in, see [`inflate`]) once focused — an unfocused row has no
/// background at all. Host rows and utility rows used to each carry their own
/// near-identical copy of this (differing only by accident of drift, in icon
/// size/padding, not by design).
#[allow(clippy::too_many_arguments)]
fn draw_sidebar_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    icon_font: &Font,
    rect: Rect,
    glyph: &str,
    label: &str,
    focused: bool,
) -> Result<()> {
    let drawn = draw_selectable(painter, rect, focused);
    let icon_size = 30u32;
    let icon_pad = 20;
    let icon_rect = Rect::new(
        drawn.x() + icon_pad,
        drawn.y() + (drawn.height() as i32 - icon_size as i32) / 2,
        icon_size,
        icon_size,
    );
    let color = if focused { WHITE } else { MUTED };
    draw_icon(painter, text_cache, icon_font, icon_rect, glyph, color)?;
    draw_text(
        painter,
        text_cache,
        font_label,
        label,
        drawn.x() + icon_pad + icon_size as i32 + 16,
        drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
        color,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_host_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    icon_font: &Font,
    rect: Rect,
    name: &str,
    paired: bool,
    focused: bool,
) -> Result<()> {
    let glyph = if paired { ICON_TV } else { ICON_LOCK };
    draw_sidebar_row(painter, text_cache, font_label, icon_font, rect, glyph, name, focused)
}

fn draw_utility_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    icon_font: &Font,
    rect: Rect,
    label: &str,
    focused: bool,
) -> Result<()> {
    let glyph = if label.starts_with('+') {
        ICON_ADD
    } else {
        ICON_SETTINGS
    };
    let label = label.trim_start_matches('+').trim();
    draw_sidebar_row(painter, text_cache, font_label, icon_font, rect, glyph, label, focused)
}

// ------------------------------------------------------------------------ grid --

pub const GRID_PAD: i32 = 32;
pub const GRID_GAP: i32 = 24;
pub const GRID_TOP_Y: i32 = 160;
pub const CARD_MIN_W: u32 = 220;

/// `clamp(2, available_w / (min_card_w + gap), 5)` — moonlight-tv's own formula.
pub fn grid_columns(available_w: u32) -> usize {
    let cols = (available_w / (CARD_MIN_W + GRID_GAP as u32)).max(1);
    cols.clamp(2, 5) as usize
}

/// 3:4 portrait aspect, matching moonlight-tv's box-art tiles.
pub fn grid_card_size(available_w: u32, columns: usize) -> (u32, u32) {
    let usable = available_w.saturating_sub(2 * GRID_PAD as u32);
    let gaps = (columns as u32).saturating_sub(1) * GRID_GAP as u32;
    let w = usable.saturating_sub(gaps) / columns.max(1) as u32;
    let h = w * 4 / 3;
    (w, h)
}

pub fn grid_card_rect(index: usize, columns: usize, grid_x: i32, available_w: u32) -> Rect {
    let (card_w, card_h) = grid_card_size(available_w, columns);
    let col = index % columns.max(1);
    let row = index / columns.max(1);
    let x = grid_x + GRID_PAD + col as i32 * (card_w as i32 + GRID_GAP);
    let y = GRID_TOP_Y + row as i32 * (card_h as i32 + GRID_GAP);
    Rect::new(x, y, card_w, card_h)
}

pub fn hit_test_grid_card(
    mouse_x: i32,
    mouse_y: i32,
    columns: usize,
    count: usize,
    grid_x: i32,
    available_w: u32,
) -> Option<usize> {
    if mouse_x < grid_x {
        return None;
    }
    (0..count).find(|&i| grid_card_rect(i, columns, grid_x, available_w).contains_point((mouse_x, mouse_y)))
}

// ----------------------------------------------------------------------- modals --

/// Dims the already-rendered frame beneath a modal (Settings/Pairing/Add host all
/// render on top of the current Home frame, then this, then their own card).
pub fn draw_modal_backdrop(painter: &mut Painter) {
    painter.dim(MODAL_SCRIM.a);
}

/// A centered glass card of `(width_frac * screen_w, height)`.
pub fn modal_card_rect(screen_w: u32, screen_h: u32, width_frac: f32, height: u32) -> Rect {
    let w = (screen_w as f32 * width_frac).round() as u32;
    let x = (screen_w as i32 - w as i32) / 2;
    let y = (screen_h as i32 - height as i32) / 2;
    Rect::new(x, y, w, height)
}

pub fn draw_modal_card(painter: &mut Painter, rect: Rect) {
    draw_card_shadow(painter, rect, MODAL_RADIUS);
    painter.fill_rounded_rect(rect, MODAL_RADIUS, SIDEBAR_BG);
    painter.stroke_rounded_rect(rect, MODAL_RADIUS, Color::RGBA(0xff, 0xff, 0xff, 0x18), 1.5);
}

/// The modal close (X) button rect, top-right inset of `card_rect`.
pub fn modal_close_rect(card_rect: Rect) -> Rect {
    const SIZE: u32 = 44;
    const MARGIN: i32 = 20;
    Rect::new(
        card_rect.x() + card_rect.width() as i32 - MARGIN - SIZE as i32,
        card_rect.y() + MARGIN,
        SIZE,
        SIZE,
    )
}

// ------------------------------------------------------------------- settings --

/// How a settings row behaves when focused/confirmed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Dropdown,
    Slider,
    Toggle,
}

pub struct SettingsRow {
    pub label: String,
    pub value: String,
    pub kind: RowKind,
    /// 0.0-1.0 fill fraction, only meaningful for `RowKind::Slider`.
    pub fraction: f32,
}

/// Resolution presets — the three the user asked for, matching `pf-console-ui`'s
/// existing 1080p/1440p/4K entries (a subset of its full list; no 720p/800p here).
pub const RESOLUTIONS: [(u32, u32, &str); 3] = [
    (1920, 1080, "1920 x 1080"),
    (2560, 1440, "2560 x 1440"),
    (3840, 2160, "3840 x 2160"),
];

/// Framerate presets — sent to the host as the exact wire refresh rate.
pub const REFRESH_RATES: [u32; 3] = [30, 60, 120];

/// Bitrate slider range/step, in kbps — the user's explicit ask ("10-150 Mbps max").
pub const BITRATE_MIN_KBPS: u32 = 10_000;
pub const BITRATE_MAX_KBPS: u32 = 150_000;
pub const BITRATE_STEP_KBPS: u32 = 5_000;
/// Sentinel one notch below `BITRATE_MIN_KBPS` on the slider: `punktfunk_core::client::NativeClient`
/// arms its own client-side AIMD bitrate controller (`punktfunk_core::abr`) precisely when it's
/// asked to connect with `bitrate_kbps == 0` — it reacts to unrecoverable frames, heavy loss,
/// one-way-delay rise, and (via `session.rs`'s `report_decode_us` call) decode latency, backing off
/// or climbing every ~750ms. A fixed Mbps number, however carefully picked, never adapts to a link
/// that degrades mid-session — this does.
pub const BITRATE_AUTOMATIC: u32 = 0;
/// Above this, aurora-tv's own moonlight-tv wiki notes stability drops off on
/// typical Wi-Fi — shown as an amber caution, matching the reference's settings
/// pane (not a hard cap, the slider still allows up to `BITRATE_MAX_KBPS`).
pub const BITRATE_WARN_KBPS: u32 = 65_000;

/// Settings-modal row indices — shared by `settings_rows`, `adjust_setting`, and
/// `app.rs`'s event handling so the mapping only lives in one place.
pub const ROW_RESOLUTION: usize = 0;
pub const ROW_FRAMERATE: usize = 1;
pub const ROW_BITRATE: usize = 2;
pub const ROW_HDR: usize = 3;
pub const SETTINGS_ROW_COUNT: usize = 4;

/// Cycles `current` to the next/previous value in a preset slice, wrapping.
pub fn cycle<T: Copy + PartialEq>(options: &[T], current: T, forward: bool) -> T {
    let idx = options.iter().position(|&o| o == current).unwrap_or(0);
    let len = options.len();
    let next = if forward {
        (idx + 1) % len
    } else {
        (idx + len - 1) % len
    };
    options[next]
}

fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

fn resolution_label(width: u32, height: u32) -> String {
    RESOLUTIONS
        .iter()
        .find(|(w, h, _)| *w == width && *h == height)
        .map_or_else(|| format!("{width}x{height}"), |(_, _, s)| s.to_string())
}

pub fn settings_rows(settings: &Settings) -> Vec<SettingsRow> {
    let bitrate_frac = if settings.bitrate_kbps == BITRATE_AUTOMATIC {
        0.0
    } else {
        (settings.bitrate_kbps.saturating_sub(BITRATE_MIN_KBPS)) as f32 / (BITRATE_MAX_KBPS - BITRATE_MIN_KBPS) as f32
    };
    vec![
        SettingsRow {
            label: "Resolution".into(),
            value: resolution_label(settings.width, settings.height),
            kind: RowKind::Dropdown,
            fraction: 0.0,
        },
        SettingsRow {
            label: "Frame rate".into(),
            value: format!("{} Hz", settings.refresh_hz),
            kind: RowKind::Dropdown,
            fraction: 0.0,
        },
        SettingsRow {
            label: "Bitrate".into(),
            value: if settings.bitrate_kbps == BITRATE_AUTOMATIC {
                "Automatic".into()
            } else {
                format!("{} Mbps", settings.bitrate_kbps / 1000)
            },
            kind: RowKind::Slider,
            fraction: bitrate_frac,
        },
        SettingsRow {
            label: "HDR".into(),
            value: if settings.hdr_enabled {
                "On".into()
            } else {
                "Off".into()
            },
            kind: RowKind::Toggle,
            fraction: 0.0,
        },
    ]
}

/// The option labels for a dropdown row (Resolution/Frame rate only).
pub fn dropdown_options(row_index: usize) -> Vec<String> {
    match row_index {
        ROW_RESOLUTION => RESOLUTIONS.iter().map(|(w, h, _)| resolution_label(*w, *h)).collect(),
        ROW_FRAMERATE => REFRESH_RATES.iter().map(|hz| format!("{hz} Hz")).collect(),
        _ => Vec::new(),
    }
}

/// Which option index in `dropdown_options(row_index)` matches the current setting.
pub fn dropdown_current_index(settings: &Settings, row_index: usize) -> usize {
    match row_index {
        ROW_RESOLUTION => RESOLUTIONS
            .iter()
            .position(|(w, h, _)| *w == settings.width && *h == settings.height)
            .unwrap_or(0),
        ROW_FRAMERATE => REFRESH_RATES
            .iter()
            .position(|hz| *hz == settings.refresh_hz)
            .unwrap_or(0),
        _ => 0,
    }
}

pub fn apply_dropdown_choice(settings: &mut Settings, row_index: usize, choice_index: usize) {
    match row_index {
        ROW_RESOLUTION => {
            if let Some((w, h, _)) = RESOLUTIONS.get(choice_index) {
                settings.width = *w;
                settings.height = *h;
            }
        }
        ROW_FRAMERATE => {
            if let Some(hz) = REFRESH_RATES.get(choice_index) {
                settings.refresh_hz = *hz;
            }
        }
        _ => {}
    }
}

/// Applies a left/right adjustment to `settings` for the given settings-row index.
/// Returns `true` if it changed.
pub fn adjust_setting(settings: &mut Settings, row_index: usize, forward: bool) -> bool {
    match row_index {
        ROW_RESOLUTION => {
            let idx = dropdown_current_index(settings, row_index);
            let next = cycle_index(idx, RESOLUTIONS.len(), forward);
            apply_dropdown_choice(settings, row_index, next);
            true
        }
        ROW_FRAMERATE => {
            settings.refresh_hz = cycle(&REFRESH_RATES, settings.refresh_hz, forward);
            true
        }
        ROW_BITRATE => {
            if settings.bitrate_kbps == BITRATE_AUTOMATIC {
                if forward {
                    settings.bitrate_kbps = BITRATE_MIN_KBPS;
                }
                // Already at the floor going backward from Automatic — nothing below it.
            } else if !forward && settings.bitrate_kbps == BITRATE_MIN_KBPS {
                settings.bitrate_kbps = BITRATE_AUTOMATIC;
            } else {
                let delta = i64::from(BITRATE_STEP_KBPS) * if forward { 1 } else { -1 };
                let next = (i64::from(settings.bitrate_kbps) + delta)
                    .clamp(i64::from(BITRATE_MIN_KBPS), i64::from(BITRATE_MAX_KBPS));
                settings.bitrate_kbps = next as u32;
            }
            true
        }
        ROW_HDR => {
            settings.hdr_enabled = !settings.hdr_enabled;
            true
        }
        _ => false,
    }
}

// Generous, TV-scale rows — each is its own focusable card (icon + label left,
// control right), consistent with the sidebar/grid's card+focus-ring language
// rather than the bare flat rows the upstream reference uses.
pub const SETTINGS_ROW_H: u32 = 92;
pub const SETTINGS_ROW_GAP: i32 = 18;
const SETTINGS_ICON_SIZE: u32 = 30;

/// Draws the settings modal's row list inside `content_rect` (the modal card's
/// interior, below its title/divider) — icon + label on the left, a dropdown
/// pill / slider / modern switch on the right. Only the focused row gets a
/// background card (see [`draw_selectable`]), zoomed in slightly, with brighter
/// icon/label/value color; an unfocused row is bare.
#[allow(clippy::too_many_arguments)]
pub fn draw_settings_rows(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    font_value: &Font,
    icon_font: &Font,
    rows: &[SettingsRow],
    focused_index: usize,
    content_rect: Rect,
) -> Result<()> {
    for (i, row) in rows.iter().enumerate() {
        let y = content_rect.y() + i as i32 * (SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP);
        let focused = i == focused_index;
        let row_rect = Rect::new(content_rect.x(), y, content_rect.width(), SETTINGS_ROW_H);
        let drawn = draw_selectable(painter, row_rect, focused);

        let icon_pad = 24;
        let icon_rect = Rect::new(
            drawn.x() + icon_pad,
            drawn.y() + (drawn.height() as i32 - SETTINGS_ICON_SIZE as i32) / 2,
            SETTINGS_ICON_SIZE,
            SETTINGS_ICON_SIZE,
        );
        let icon_color = if focused { WHITE } else { MUTED };
        let glyph = match row.kind {
            RowKind::Dropdown if i == ROW_RESOLUTION => ICON_MONITOR,
            RowKind::Dropdown => ICON_SCHEDULE,
            RowKind::Slider => ICON_SIGNAL,
            RowKind::Toggle => ICON_SUN,
        };
        draw_icon(painter, text_cache, icon_font, icon_rect, glyph, icon_color)?;
        let label_x = icon_rect.x() + SETTINGS_ICON_SIZE as i32 + 20;
        draw_text(
            painter,
            text_cache,
            font_label,
            &row.label,
            label_x,
            drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
            if focused { WHITE } else { MUTED },
        )?;

        let control_pad = 28;
        match row.kind {
            RowKind::Dropdown => {
                let pill_w = 220u32.min(drawn.width() / 2);
                let pill = Rect::new(
                    drawn.x() + drawn.width() as i32 - control_pad - pill_w as i32,
                    drawn.y() + (drawn.height() as i32 - 52) / 2,
                    pill_w,
                    52,
                );
                draw_dropdown_pill(painter, text_cache, font_value, icon_font, pill, &row.value, focused)?;
            }
            RowKind::Slider => {
                let value_w = font_value.size_of(&row.value).map_or(0, |(w, _)| w);
                let track_w = 220u32.min(drawn.width() / 3);
                let value_x = drawn.x() + drawn.width() as i32 - control_pad - value_w as i32;
                draw_text(
                    painter,
                    text_cache,
                    font_value,
                    &row.value,
                    value_x,
                    drawn.y() + (drawn.height() as i32 - font_value.height()) / 2,
                    if focused { WHITE } else { MUTED },
                )?;
                let track = Rect::new(
                    value_x - 24 - track_w as i32,
                    drawn.y() + (drawn.height() as i32 - 10) / 2,
                    track_w,
                    10,
                );
                draw_slider_with_thumb(painter, track, row.fraction, focused);
            }
            RowKind::Toggle => {
                let switch = Rect::new(
                    drawn.x() + drawn.width() as i32 - control_pad - 64,
                    drawn.y() + (drawn.height() as i32 - 34) / 2,
                    64,
                    34,
                );
                draw_switch(painter, switch, row.value == "On");
            }
        }
    }
    Ok(())
}

/// The Wake modal's two rows — "Send Wake-on-LAN now" and the "Always send
/// automatically" toggle (see `app::WakeState`) — drawn with the same icon +
/// label + control row language as `draw_settings_rows`, but as a fixed pair
/// rather than a data-driven list, since these rows live outside the Settings
/// screen. `content` is the first row's rect; the second is stacked directly
/// below it using `SETTINGS_ROW_GAP`, same as `draw_settings_rows`.
#[allow(clippy::too_many_arguments)]
pub fn draw_wake_rows(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    icon_font: &Font,
    content: Rect,
    send_label: &str,
    focused_index: usize,
    auto_send: bool,
) -> Result<()> {
    let icon_pad = 24;
    let control_pad = 28;
    for (i, label) in [send_label, "Always send automatically"].into_iter().enumerate() {
        let row_rect = Rect::new(
            content.x(),
            content.y() + i as i32 * (SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP),
            content.width(),
            SETTINGS_ROW_H,
        );
        let focused = i == focused_index;
        let drawn = draw_selectable(painter, row_rect, focused);
        let color = if focused { WHITE } else { MUTED };
        let icon_rect = Rect::new(
            drawn.x() + icon_pad,
            drawn.y() + (drawn.height() as i32 - SETTINGS_ICON_SIZE as i32) / 2,
            SETTINGS_ICON_SIZE,
            SETTINGS_ICON_SIZE,
        );
        draw_icon(painter, text_cache, icon_font, icon_rect, ICON_POWER, color)?;
        draw_text(
            painter,
            text_cache,
            font_label,
            label,
            icon_rect.x() + SETTINGS_ICON_SIZE as i32 + 20,
            drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
            color,
        )?;
        // Only the second row ("Always send automatically") has a control.
        if i == 1 {
            let switch = Rect::new(
                drawn.x() + drawn.width() as i32 - control_pad - 64,
                drawn.y() + (drawn.height() as i32 - 34) / 2,
                64,
                34,
            );
            draw_switch(painter, switch, auto_send);
        }
    }
    Ok(())
}

/// A rounded pill button showing the current dropdown value + a small chevron
/// (`ICON_CHEVRON_DOWN`, replacing a hand-drawn triangle — see the icons section).
pub fn draw_dropdown_pill(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font: &Font,
    icon_font: &Font,
    rect: Rect,
    label: &str,
    focused: bool,
) -> Result<()> {
    let radius = rect.height() as i32 / 2;
    painter.fill_rounded_rect(rect, radius, Color::RGBA(0xff, 0xff, 0xff, 0x12));
    painter.stroke_rounded_rect(
        rect,
        radius,
        if focused {
            ACCENT_BRIGHT
        } else {
            Color::RGBA(0xff, 0xff, 0xff, 0x30)
        },
        1.5,
    );
    let chevron_size = 20u32;
    let chevron_pad = 16;
    let chevron_rect = Rect::new(
        rect.x() + rect.width() as i32 - chevron_pad - chevron_size as i32,
        rect.y() + (rect.height() as i32 - chevron_size as i32) / 2,
        chevron_size,
        chevron_size,
    );
    draw_icon(painter, text_cache, icon_font, chevron_rect, ICON_CHEVRON_DOWN, WHITE)?;
    let text_w = font.size_of(label).map_or(0, |(w, _)| w);
    let text_x = rect.x() + ((rect.width() as i32 - chevron_size as i32 - chevron_pad) - text_w as i32) / 2;
    draw_text(
        painter,
        text_cache,
        font,
        label,
        text_x.max(rect.x()),
        rect.y() + (rect.height() as i32 - font.height()) / 2,
        WHITE,
    )?;
    Ok(())
}

/// A round-thumbed slider track, shadowed knob (matches the reference's
/// slider-knob-shadow theme touch).
pub fn draw_slider_with_thumb(painter: &mut Painter, rect: Rect, fraction: f32, focused: bool) {
    let track_h = rect.height();
    painter.fill_rounded_rect(rect, track_h as i32 / 2, Color::RGBA(0xff, 0xff, 0xff, 0x22));
    let filled_w = (rect.width() as f32 * fraction.clamp(0.0, 1.0)) as u32;
    if filled_w > 0 {
        let filled = Rect::new(rect.x(), rect.y(), filled_w.max(track_h), track_h);
        painter.fill_rounded_rect(filled, track_h as i32 / 2, ACCENT);
    }
    let thumb_r = 14.0;
    let cx = rect.x() as f32 + filled_w as f32;
    let cy = rect.y() as f32 + rect.height() as f32 / 2.0;
    painter.fill_circle(cx + 2.0, cy + 3.0, thumb_r, Color::RGBA(0x00, 0x00, 0x00, 0x50));
    painter.fill_circle(cx, cy, thumb_r, if focused { WHITE } else { MUTED });
}

/// A modern sliding pill switch (iOS/Android-style) — accent-filled track with
/// the knob at the right when on, muted track with the knob at the left when
/// off.
pub fn draw_switch(painter: &mut Painter, rect: Rect, on: bool) {
    let radius = rect.height() as i32 / 2;
    painter.fill_rounded_rect(
        rect,
        radius,
        if on {
            ACCENT
        } else {
            Color::RGBA(0xff, 0xff, 0xff, 0x22)
        },
    );
    let knob_r = radius as f32 - 4.0;
    let cy = rect.y() as f32 + rect.height() as f32 / 2.0;
    let cx = if on {
        rect.x() as f32 + rect.width() as f32 - radius as f32
    } else {
        rect.x() as f32 + radius as f32
    };
    painter.fill_circle(cx + 1.0, cy + 2.0, knob_r, Color::RGBA(0x00, 0x00, 0x00, 0x40));
    painter.fill_circle(cx, cy, knob_r, WHITE);
}

/// Renders a dropdown's options as an overlay list anchored just below the row that
/// opened it, inside the settings modal card. One shadow/background for the whole
/// panel and contiguous, same-height rows — like a typical dropdown/picker list —
/// rather than every row being its own floating `draw_card` (which used to stack a
/// drop shadow under each option a few px apart from its neighbors, reading as a
/// stray smear between rows instead of a clean list).
pub fn draw_dropdown_overlay(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_value: &Font,
    options: &[String],
    focused_index: usize,
    rect: Rect,
) -> Result<()> {
    let row_h = 56u32;
    let bg_rect = Rect::new(rect.x(), rect.y(), rect.width(), options.len() as u32 * row_h);
    draw_popup_panel(painter, bg_rect, Color::RGBA(0xff, 0xff, 0xff, 0x20));
    for (i, opt) in options.iter().enumerate() {
        let row_rect = Rect::new(rect.x(), rect.y() + i as i32 * row_h as i32, rect.width(), row_h);
        let focused = i == focused_index;
        if focused {
            let highlight = Rect::new(
                row_rect.x() + 6,
                row_rect.y() + 4,
                row_rect.width().saturating_sub(12),
                row_rect.height().saturating_sub(8),
            );
            painter.fill_rounded_rect(highlight, 8, Color::RGBA(ACCENT.r, ACCENT.g, ACCENT.b, 0x50));
        }
        draw_text(
            painter,
            text_cache,
            font_value,
            opt,
            row_rect.x() + 20,
            row_rect.y() + (row_rect.height() as i32 - font_value.height()) / 2,
            if focused { WHITE } else { MUTED },
        )?;
    }
    Ok(())
}

/// The floating-panel chrome shared by every popup menu drawn over Home/the
/// modals — a shadowed, near-black rounded panel with a colored border.
/// Extracted from [`draw_dropdown_overlay`], which used to carry its own copy
/// of this same triple (shadow, fill, stroke).
fn draw_popup_panel(painter: &mut Painter, rect: Rect, border_color: Color) {
    draw_card_shadow(painter, rect, CARD_RADIUS);
    painter.fill_rounded_rect(rect, CARD_RADIUS, Color::RGBA(0x10, 0x10, 0x10, 0xf0));
    painter.stroke_rounded_rect(rect, CARD_RADIUS, border_color, 1.5);
}

/// One button in a [`draw_confirm_buttons`] row — `color` is that button's own
/// identity color, shown at full strength only while it has focus (unfocused
/// buttons dim to [`MUTED`], the same "unfocused = muted" convention every
/// other focusable row in this UI already uses).
pub struct ConfirmButton<'a> {
    pub icon: Option<&'a str>,
    pub label: &'a str,
    pub color: Color,
}

/// A row of side-by-side buttons for a Yes/No-style confirmation (currently
/// just the "Forget this host?" dialog's Forget/Cancel pair, but not written
/// specifically for that) — an optional leading icon and a label colored by
/// that button's own identity when focused, or [`MUTED`] otherwise. The focused
/// button (only) gets a background card, zoomed in slightly (see
/// [`draw_selectable`]). `focused_index` picks which of `buttons` has focus.
pub fn draw_confirm_buttons(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    icon_font: &Font,
    content: Rect,
    buttons: &[ConfirmButton; 2],
    focused_index: usize,
) -> Result<()> {
    let gap = 20i32;
    let btn_w = content.width().saturating_sub(gap as u32) / 2;
    for (i, button) in buttons.iter().enumerate() {
        let rect = Rect::new(
            content.x() + i as i32 * (btn_w as i32 + gap),
            content.y(),
            btn_w,
            content.height(),
        );
        let focused = i == focused_index;
        let drawn = draw_selectable(painter, rect, focused);
        let color = if focused { button.color } else { MUTED };

        let label_w = font_label.size_of(button.label).map_or(0, |(w, _)| w);
        let text_x = match button.icon {
            Some(icon) => {
                let icon_size = 26u32;
                let icon_rect = Rect::new(
                    drawn.x() + 20,
                    drawn.y() + (drawn.height() as i32 - icon_size as i32) / 2,
                    icon_size,
                    icon_size,
                );
                draw_icon(painter, text_cache, icon_font, icon_rect, icon, color)?;
                icon_rect.x() + icon_size as i32 + 12
            }
            // No icon: center the label instead of left-aligning it after one.
            None => drawn.x() + (drawn.width() as i32 - label_w as i32) / 2,
        };
        draw_text(
            painter,
            text_cache,
            font_label,
            button.label,
            text_x,
            drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
            color,
        )?;
    }
    Ok(())
}

// -------------------------------------------------------------------- add host --

/// punktfunk's conventional host port (see `store::dev_override_connect`'s
/// fallback) — fixed and not user-editable, so the add-host screen only ever
/// has to ask for an IP address.
pub const FIXED_HOST_PORT: u16 = 9777;

/// Manual "add host by IP" entry state: a plain, naturally-growing digit
/// string rather than a fixed-width masked grid — no `_` placeholders, no
/// per-octet box, no port field (that's always [`FIXED_HOST_PORT`]). Dots are
/// inserted automatically once an octet is complete (three digits, or a
/// fourth that would push its value past 255), so the Magic Remote's number
/// pad (`digit_key_value`) — the only realistic input this screen gets — is
/// enough on its own, with Left/Right (see `app.rs`'s `handle_add_host_event`)
/// standing in for backspace/"next octet" on a remote with no dot key.
#[derive(Default)]
pub struct AddHostState {
    /// Completed octets so far (0-3 of them once a further one is being typed).
    octets: Vec<u8>,
    /// Digits typed into the octet currently being entered, not yet finalized
    /// into `octets` — kept as text (not a parsed `u8`) so it can grow one
    /// digit at a time and still show a partial value like "2" or "25".
    current: String,
}

impl AddHostState {
    /// Whether exactly four octets' worth of digits have been typed — the
    /// point at which `host_and_port()` names a real, connectable address.
    pub fn is_complete(&self) -> bool {
        (self.octets.len() == 4 && self.current.is_empty()) || (self.octets.len() == 3 && !self.current.is_empty())
    }

    pub fn host_and_port(&self) -> (String, u16) {
        let mut parts: Vec<String> = self.octets.iter().map(u8::to_string).collect();
        if !self.current.is_empty() {
            parts.push(self.current.clone());
        }
        (parts.join("."), FIXED_HOST_PORT)
    }

    /// What's actually been typed so far, exactly as typed — no mask, no
    /// placeholders, no port.
    pub fn display_text(&self) -> String {
        self.host_and_port().0
    }

    /// Types one digit (0-9) into the octet currently being entered, finishing
    /// it automatically (a dot appears) once it hits three digits or a fourth
    /// digit would push its value past 255 — the same auto-advance idiom as a
    /// phone's IP-entry field, needed since the remote has no dot key of its own.
    pub fn enter_digit(&mut self, digit: u8) {
        if self.octets.len() >= 4 {
            return;
        }
        let mut candidate = self.current.clone();
        candidate.push((b'0' + digit) as char);
        let value: u32 = candidate.parse().unwrap_or(0);
        if value > 255 || candidate.len() > 3 {
            self.advance_octet();
            if self.octets.len() < 4 {
                self.current.push((b'0' + digit) as char);
            }
            return;
        }
        self.current = candidate;
        if self.current.len() == 3 {
            self.advance_octet();
        }
    }

    /// Deletes the last typed character — a digit from the in-progress octet,
    /// or (once that's empty) undoes the last completed octet back into it for
    /// editing. Left on the d-pad.
    pub fn backspace(&mut self) {
        if !self.current.is_empty() {
            self.current.pop();
        } else if let Some(last) = self.octets.pop() {
            self.current = last.to_string();
        }
    }

    /// Manually finishes the octet in progress — so e.g. "8" can become
    /// "8.8.8.8" without waiting for three digits or an overflow. Right on the
    /// d-pad, standing in for the "." key a real keyboard would have.
    pub fn advance_octet(&mut self) {
        if self.current.is_empty() || self.octets.len() >= 4 {
            return;
        }
        let value: u8 = self.current.parse().unwrap_or(0);
        self.octets.push(value);
        self.current.clear();
    }
}
