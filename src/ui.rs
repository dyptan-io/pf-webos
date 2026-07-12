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

/// Raw SDL2 scancode values the `webosbrew/SDL-webOS` fork (the one this client
/// already links against for its Wayland shell-integration protocol — see Cargo.toml
/// docs) assigns to Magic Remote keys with no vanilla-SDL2 equivalent
/// (`include/SDL_scancode.h`). Vanilla SDL2 doesn't define these, and rust-sdl2's
/// `Scancode` enum only covers vanilla SDL2's scancode set — so they're
/// unrepresentable as a `Scancode` value and don't arrive as `Event::KeyDown`/
/// `KeyUp` at all through the safe event API. Reading them requires the raw
/// keyboard-state array instead (see `scancode_down`).
const SCANCODE_WEBOS_BACK: usize = 482;
/// Translated from the X11 keycode 406 sourced from `/usr/share/X11/xkb/keycodes/lg`.
const SCANCODE_WEBOS_RED: usize = 486;

/// Polls the current (level, not edge) state of a raw webOS scancode directly from
/// SDL2's keyboard-state array, bypassing rust-sdl2's `Scancode` enum entirely (see
/// `SCANCODE_WEBOS_BACK`/`SCANCODE_WEBOS_RED` docs for why). The caller
/// edge-detects a press from consecutive polls.
fn scancode_down(scancode: usize) -> bool {
    unsafe {
        let mut numkeys = 0;
        let ptr = sdl2::sys::SDL_GetKeyboardState(&mut numkeys);
        if ptr.is_null() {
            return false;
        }
        let state = std::slice::from_raw_parts(ptr, numkeys as usize);
        state.get(scancode).copied().unwrap_or(0) != 0
    }
}

/// The Magic Remote's actual Back button — reaches the app at all only because
/// `main.rs` sets the `SDL_WEBOS_ACCESS_POLICY_KEYS_BACK` hint before window
/// creation (otherwise webOS's system launcher intercepts it first, backgrounding
/// the app instead of delivering a key event). Even with that hint set, the key
/// arrives as this raw scancode, not a `Scancode`/`Keycode` the safe event API
/// recognizes (see `SCANCODE_WEBOS_BACK`'s docs) — same situation as the color
/// buttons below.
pub fn webos_back_button_down() -> bool {
    scancode_down(SCANCODE_WEBOS_BACK)
}

/// Polls the current (level, not edge) state of the Magic Remote's Red button —
/// kept as a secondary Back/disconnect trigger alongside the real Back button
/// (`webos_back_button_down`) since the access-policy hint isn't honored
/// consistently across every firmware/model (see `docs/NOTES.md`).
pub fn webos_red_button_down() -> bool {
    scancode_down(SCANCODE_WEBOS_RED)
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

/// A flat-color, anti-aliased `Paint` — every fill/stroke in this module uses one
/// of these and nothing fancier (no gradients/patterns needed for this UI).
fn solid_paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(sk_color(color));
    paint.anti_alias = true;
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
}

impl Painter {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pixmap: Pixmap::new(width.max(1), height.max(1)).expect("nonzero framebuffer size"),
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
    pub fn fill_shadow(&mut self, rect: Rect, radius: i32, dx: f32, dy: f32, blur: f32, opacity: u8) {
        draw_soft_shadow(&mut self.pixmap, rect, radius, dx, dy, blur, opacity);
    }

    pub fn draw_pixmap(&mut self, x: i32, y: i32, src: &Pixmap) {
        self.pixmap
            .draw_pixmap(x, y, src.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }

    /// Composites `src` scaled to exactly fill `dst` — `image`-decoded cover art
    /// (see `art.rs`) is already downscaled close to display size, so bilinear here
    /// is just a small final-fit correction, not doing the heavy lifting of the
    /// downscale.
    pub fn draw_pixmap_scaled(&mut self, dst: Rect, src: &Pixmap) {
        let (dw, dh) = (dst.width() as f32, dst.height() as f32);
        let (sw, sh) = (src.width() as f32, src.height() as f32);
        if dw <= 0.0 || dh <= 0.0 || sw <= 0.0 || sh <= 0.0 {
            return;
        }
        let transform = Transform::from_scale(dw / sw, dh / sh).post_translate(dst.x() as f32, dst.y() as f32);
        let paint = PixmapPaint {
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        self.pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    }
}

/// How far a shadow's blur extends past the shape casting it, in px — a fixed
/// constant (not derived from anything) picked to read as a soft TV-scale shadow.
const SHADOW_BLUR: f32 = 14.0;

/// Rasterizes `rect`'s rounded-rect shape into a small padded alpha buffer, box-blurs
/// it (3 passes — a cheap approximation of a Gaussian blur, good enough at TV
/// viewing distance for a drop shadow), then composites it as a black shadow offset
/// by `(dx, dy)`.
fn draw_soft_shadow(dst: &mut Pixmap, rect: Rect, radius: i32, dx: f32, dy: f32, blur: f32, opacity: u8) {
    let pad = blur.ceil().max(0.0) as i32 + 1;
    let (w, h) = (rect.width() as i32 + 2 * pad, rect.height() as i32 + 2 * pad);
    if w <= 0 || h <= 0 {
        return;
    }
    let Some(mut shape) = Pixmap::new(w as u32, h as u32) else {
        return;
    };
    let Some(path) = rounded_rect_path(
        pad as f32,
        pad as f32,
        rect.width() as f32,
        rect.height() as f32,
        radius as f32,
    ) else {
        return;
    };
    let paint = solid_paint(Color::RGBA(0, 0, 0, opacity));
    shape.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);

    // tiny-skia stores premultiplied RGBA; a pure-black shape's R/G/B channels are
    // always 0, so its alpha channel alone fully describes the shape — blur that
    // channel directly rather than blurring all 4 for no visual difference.
    let mut alpha: Vec<u8> = shape.data().iter().skip(3).step_by(4).copied().collect();
    let radius_px = (blur / 2.0).round().max(1.0) as usize;
    for _ in 0..3 {
        box_blur(&mut alpha, w as usize, h as usize, radius_px);
    }
    for (i, a) in alpha.into_iter().enumerate() {
        shape.data_mut()[i * 4 + 3] = a; // R/G/B stay 0 (premultiplied black)
    }

    dst.draw_pixmap(
        rect.x() - pad + dx.round() as i32,
        rect.y() - pad + dy.round() as i32,
        shape.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
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

/// moonlight-tv's focus cue is an outline ring offset outward from the tile, not a
/// filled/background change — bright accent blue, invisible unless focused. Two
/// passes at increasing offset/decreasing alpha approximate a soft glow.
pub fn draw_focus_ring(painter: &mut Painter, rect: Rect, radius: i32) {
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

/// A soft, real drop shadow (see [`Painter::fill_shadow`]) — matches the reference's
/// shadowed-card look.
fn draw_card_shadow(painter: &mut Painter, rect: Rect, radius: i32) {
    painter.fill_shadow(rect, radius, 3.0, 5.0, SHADOW_BLUR, 0x60);
}

/// Draws a plain surface card (sidebar rows, settings rows, PIN/IP digit boxes) —
/// shadow, `SIDEBAR_BG` fill, and a focus ring when focused. Returns the (possibly
/// zoom-inflated) rect actually drawn, so callers can center content inside it.
pub fn draw_card(painter: &mut Painter, rect: Rect, focused: bool) -> Rect {
    let r = inflate(rect, focused);
    draw_card_shadow(painter, r, CARD_RADIUS);
    painter.fill_rounded_rect(r, CARD_RADIUS, SIDEBAR_BG);
    if focused {
        draw_focus_ring(painter, r, CARD_RADIUS);
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

/// `None` when `(x, y)` falls outside the sidebar's horizontal band at all — lets
/// mouse-motion handling distinguish "not hovering the sidebar" from "hovering the
/// sidebar but between rows."
pub fn hit_test_sidebar_row(x: i32, y: i32, row_count: usize) -> Option<usize> {
    if x < 0 || x as u32 > SIDEBAR_W {
        return None;
    }
    (0..row_count).find(|&i| sidebar_row_rect(i).contains_point((x, y)))
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
/// the top, one row per host (icon reflects paired/not-paired), then trailing
/// "+ Add host"/"Settings" utility rows. `focused_index` is `Some` only when
/// sidebar itself has focus (see `app.rs`'s `HomeFocus`).
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
            i,
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
        add_row,
        "+ Add host",
        focused_index == Some(add_row),
    )?;
    draw_utility_row(
        painter,
        text_cache,
        font_label,
        icon_font,
        settings_row,
        "Settings",
        focused_index == Some(settings_row),
    )?;

    if entries.is_empty() {
        draw_text(
            painter,
            text_cache,
            font_label,
            "No hosts yet.",
            SIDEBAR_PAD,
            SIDEBAR_TOP_Y - 32,
            MUTED,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn draw_host_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    icon_font: &Font,
    index: usize,
    name: &str,
    paired: bool,
    focused: bool,
) -> Result<()> {
    let rect = sidebar_row_rect(index);
    let drawn = draw_card(painter, rect, focused);
    let icon_size = 32u32;
    let icon_rect = Rect::new(
        drawn.x() + 18,
        drawn.y() + (drawn.height() as i32 - icon_size as i32) / 2,
        icon_size,
        icon_size,
    );
    let icon_color = if focused { WHITE } else { MUTED };
    let glyph = if paired { ICON_TV } else { ICON_LOCK };
    draw_icon(painter, text_cache, icon_font, icon_rect, glyph, icon_color)?;
    draw_text(
        painter,
        text_cache,
        font_label,
        name,
        drawn.x() + 18 + icon_size as i32 + 16,
        drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
        if focused { WHITE } else { MUTED },
    )?;
    Ok(())
}

fn draw_utility_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_label: &Font,
    icon_font: &Font,
    index: usize,
    label: &str,
    focused: bool,
) -> Result<()> {
    let rect = sidebar_row_rect(index);
    let drawn = draw_card(painter, rect, focused);
    let icon_size = 28u32;
    let icon_rect = Rect::new(
        drawn.x() + 20,
        drawn.y() + (drawn.height() as i32 - icon_size as i32) / 2,
        icon_size,
        icon_size,
    );
    let icon_color = if focused { WHITE } else { MUTED };
    let glyph = if label.starts_with('+') {
        ICON_ADD
    } else {
        ICON_SETTINGS
    };
    draw_icon(painter, text_cache, icon_font, icon_rect, glyph, icon_color)?;
    draw_text(
        painter,
        text_cache,
        font_label,
        label.trim_start_matches('+').trim(),
        drawn.x() + 20 + icon_size as i32 + 16,
        drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
        if focused { WHITE } else { MUTED },
    )?;
    Ok(())
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
pub fn draw_modal_backdrop(painter: &mut Painter, screen_w: u32, screen_h: u32) {
    painter.fill_rect(Rect::new(0, 0, screen_w, screen_h), MODAL_SCRIM);
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

/// Framerate presets. The wire value gets the aurora-tv NTSC floor-correction
/// applied on top of these nominal numbers — see `main.rs`.
pub const REFRESH_RATES: [u32; 3] = [30, 60, 120];

/// Bitrate slider range/step, in kbps — the user's explicit ask ("10-150 Mbps max").
pub const BITRATE_MIN_KBPS: u32 = 10_000;
pub const BITRATE_MAX_KBPS: u32 = 150_000;
pub const BITRATE_STEP_KBPS: u32 = 5_000;
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
    let bitrate_frac =
        (settings.bitrate_kbps.saturating_sub(BITRATE_MIN_KBPS)) as f32 / (BITRATE_MAX_KBPS - BITRATE_MIN_KBPS) as f32;
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
            value: format!("{} Mbps", settings.bitrate_kbps / 1000),
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
            let delta = i64::from(BITRATE_STEP_KBPS) * if forward { 1 } else { -1 };
            let next = (i64::from(settings.bitrate_kbps) + delta)
                .clamp(i64::from(BITRATE_MIN_KBPS), i64::from(BITRATE_MAX_KBPS));
            settings.bitrate_kbps = next as u32;
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
/// pill / slider / modern switch on the right. Each row is its own card with a
/// focus ring, matching this client's sidebar/grid visual language.
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
        let drawn = draw_card(painter, row_rect, focused);

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
        let drawn = draw_card(painter, row_rect, focused);
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
/// opened it, inside the settings modal card.
pub fn draw_dropdown_overlay(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_value: &Font,
    options: &[String],
    focused_index: usize,
    rect: Rect,
) -> Result<()> {
    let row_h = 56u32;
    let gap = 6i32;
    let overlay_h = options.len() as i32 * (row_h as i32 + gap);
    let bg_rect = Rect::new(rect.x(), rect.y(), rect.width(), overlay_h.max(0) as u32);
    painter.fill_rounded_rect(bg_rect, CARD_RADIUS, Color::RGBA(0x10, 0x10, 0x10, 0xf0));
    painter.stroke_rounded_rect(bg_rect, CARD_RADIUS, Color::RGBA(0xff, 0xff, 0xff, 0x20), 1.5);
    for (i, opt) in options.iter().enumerate() {
        let y = rect.y() + i as i32 * (row_h as i32 + gap);
        let row_rect = Rect::new(rect.x(), y, rect.width(), row_h);
        let focused = i == focused_index;
        let drawn = draw_card(painter, row_rect, focused);
        draw_text(
            painter,
            text_cache,
            font_value,
            opt,
            drawn.x() + 20,
            drawn.y() + (drawn.height() as i32 - font_value.height()) / 2,
            if focused { WHITE } else { MUTED },
        )?;
    }
    Ok(())
}

// -------------------------------------------------------------------- add host --

/// Digit-entry state for manually adding a host by IP:port — the same "type digits,
/// auto-advance" idiom the Pairing screen's PIN entry already uses (see
/// `digit_key_value`), extended to 17 slots: four 3-digit IP octets + a 5-digit port.
pub struct AddHostState {
    pub digits: [u8; 17],
    pub index: usize,
    /// Which slots the user has actually entered (typed a digit, or
    /// left/right-adjusted one) — untouched IP-octet slots render as a blank
    /// placeholder (`_`) rather than a misleading literal `0`. The port slots
    /// start touched since `9777` is a real, usable default, not a placeholder.
    touched: [bool; 17],
}

impl Default for AddHostState {
    fn default() -> Self {
        // Prefills punktfunk's conventional default port (9777 — see
        // `store::dev_override_connect`'s fallback) so the user only has to dial in
        // the IP address.
        let mut touched = [false; 17];
        touched[12..17].fill(true);
        Self {
            digits: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 7, 7, 7],
            index: 0,
            touched,
        }
    }
}

impl AddHostState {
    fn octet(&self, i: usize) -> u8 {
        let d = &self.digits[i * 3..i * 3 + 3];
        (u32::from(d[0]) * 100 + u32::from(d[1]) * 10 + u32::from(d[2])).min(255) as u8
    }

    fn port_value(&self) -> u16 {
        let v = self.digits[12..17]
            .iter()
            .fold(0u32, |acc, &digit| acc * 10 + u32::from(digit));
        v.min(u32::from(u16::MAX)) as u16
    }

    pub fn host_and_port(&self) -> (String, u16) {
        (
            format!(
                "{}.{}.{}.{}",
                self.octet(0),
                self.octet(1),
                self.octet(2),
                self.octet(3)
            ),
            self.port_value(),
        )
    }

    /// Marks the currently-indexed slot as entered — called whenever a digit
    /// key or Left/Right actually sets a value, not on plain Up/Down navigation.
    pub fn touch_current(&mut self) {
        self.touched[self.index] = true;
    }

    /// Maps `index` (0-16) to the character position in `display_text()`'s rendered
    /// string, so the UI can highlight the digit currently being edited — each
    /// 3-digit group is followed by one separator character (`.` x3, then `:`).
    pub fn focus_char_index(&self) -> usize {
        if self.index < 12 {
            (self.index / 3) * 4 + (self.index % 3)
        } else {
            16 + (self.index - 12)
        }
    }

    fn ch(&self, i: usize) -> char {
        if self.touched[i] {
            (b'0' + self.digits[i]) as char
        } else {
            '_'
        }
    }

    pub fn display_text(&self) -> String {
        format!(
            "{}{}{}.{}{}{}.{}{}{}.{}{}{}:{}{}{}{}{}",
            self.ch(0),
            self.ch(1),
            self.ch(2),
            self.ch(3),
            self.ch(4),
            self.ch(5),
            self.ch(6),
            self.ch(7),
            self.ch(8),
            self.ch(9),
            self.ch(10),
            self.ch(11),
            self.ch(12),
            self.ch(13),
            self.ch(14),
            self.ch(15),
            self.ch(16),
        )
    }
}

/// Draws `text` left-aligned at `(x, y)`, rendering the character at `focus_char` in
/// `focus_color` and every other character in `base_color` — used by the add-host
/// screen to show which digit Left/Right/number-keys currently edit.
#[allow(clippy::too_many_arguments)]
pub fn draw_highlighted_text(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font: &Font,
    text: &str,
    focus_char: usize,
    x: i32,
    y: i32,
    base_color: Color,
    focus_color: Color,
) -> Result<()> {
    let mut cursor_x = x;
    for (i, ch) in text.chars().enumerate() {
        let s = ch.to_string();
        let color = if i == focus_char { focus_color } else { base_color };
        let w = draw_text(painter, text_cache, font, &s, cursor_x, y, color)?;
        cursor_x += w as i32;
    }
    Ok(())
}
