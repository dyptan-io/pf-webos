//! Drawing/input-mapping primitives for the pre-stream UI: a persistent sidebar
//! (known hosts + Add host/Settings) beside a detail grid (the selected host's
//! games), plus centered modal cards for Pairing/Settings/Add host — modeled on
//! `mariotaku/moonlight-tv`'s actual layout and dark palette (sidebar + app grid,
//! outline-ring focus, near-square cards), reimplemented with plain SDL2 2D
//! primitives (rects + `SDL2_ttf` text — no LVGL/Skia/Vulkan available here).
//! Icons are vector-drawn, not a bundled icon font — this client bundles no
//! fonts/assets, and the system font lacks the relevant glyphs anyway.
use anyhow::{Context, Result};
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, TextureCreator};
use sdl2::ttf::Font;
use sdl2::video::{Window, WindowContext};

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

/// Raw SDL2 scancode value the `webosbrew/SDL-webOS` fork (the one this client
/// already links against for its Wayland shell-integration protocol — see Cargo.toml
/// docs) assigns to the Magic Remote's Red button (`include/SDL_scancode.h`'s
/// `SDL_SCANCODE_WEBOS_RED`, translated from the X11 keycode 406 sourced from
/// `/usr/share/X11/xkb/keycodes/lg`). Vanilla SDL2 doesn't define this, and
/// rust-sdl2's `Scancode` enum only covers vanilla SDL2's scancode set — so it's
/// unrepresentable as a `Scancode` value and doesn't arrive as `Event::KeyDown`/
/// `KeyUp` at all through the safe event API. Reading it requires the raw
/// keyboard-state array instead (see `webos_red_button_down`).
const SCANCODE_WEBOS_RED: usize = 486;

/// Polls the current (level, not edge) state of the Magic Remote's Red button
/// directly from SDL2's raw keyboard-state array, bypassing rust-sdl2's `Scancode`
/// enum entirely (see `SCANCODE_WEBOS_RED` docs for why). The caller is
/// responsible for edge-detecting a press from consecutive polls (see `main.rs`,
/// which uses Red as the reliable Back-button substitute).
pub fn webos_red_button_down() -> bool {
    unsafe {
        let mut numkeys = 0;
        let ptr = sdl2::sys::SDL_GetKeyboardState(&mut numkeys);
        if ptr.is_null() {
            return false;
        }
        let state = std::slice::from_raw_parts(ptr, numkeys as usize);
        state.get(SCANCODE_WEBOS_RED).copied().unwrap_or(0) != 0
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
    let scaled = (design_size as u32 * height_px / 720).max(10) as u16;
    ttf.load_font(SYSTEM_FONT_PATH, scaled)
        .map_err(|e| anyhow::anyhow!("load_font {SYSTEM_FONT_PATH}: {e}"))
}

/// Renders one line of text left-aligned at `(x, y)` (top-left), returning its width.
pub fn draw_text(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
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
    let texture = texture_creator
        .create_texture_from_surface(&surface)
        .context("texture from surface")?;
    let sdl2::render::TextureQuery { width, height, .. } = texture.query();
    canvas
        .copy(&texture, None, Rect::new(x, y, width, height))
        .map_err(|e| anyhow::anyhow!("copy text texture: {e}"))?;
    Ok(width)
}

/// Truncates `text` with a trailing "…" so it fits within `max_w` pixels in `font`
/// (moonlight-tv scroll-marquees long titles on focus instead — see the module docs
/// on why this client keeps it simple).
pub fn ellipsize(font: &Font, text: &str, max_w: u32) -> String {
    if font.size_of(text).map(|(w, _)| w).unwrap_or(0) <= max_w {
        return text.to_string();
    }
    let mut s: Vec<char> = text.chars().collect();
    while !s.is_empty() {
        s.pop();
        let candidate: String = s.iter().collect::<String>() + "…";
        if font.size_of(&candidate).map(|(w, _)| w).unwrap_or(0) <= max_w {
            return candidate;
        }
    }
    "…".to_string()
}

// ------------------------------------------------------------------- primitives --

/// Fills a rounded rectangle one scanline at a time (a plain filled rect in the
/// middle, an inset computed per-row from the corner circle's equation near the
/// top/bottom edges) — no SDL2_gfx dependency, cheap enough for the handful of
/// panels on screen at once.
pub fn fill_rounded_rect(canvas: &mut Canvas<Window>, rect: Rect, radius: i32, color: Color) {
    let (w, h) = (rect.width() as i32, rect.height() as i32);
    let r = radius.max(0).min(h / 2).min(w / 2);
    canvas.set_draw_color(color);
    if r == 0 || w <= 0 || h <= 0 {
        let _ = canvas.fill_rect(rect);
        return;
    }
    for y in 0..h {
        let dy = if y < r {
            r - 1 - y
        } else if y >= h - r {
            y - (h - r)
        } else {
            -1
        };
        let inset = if dy < 0 { 0 } else { r - (((r * r - dy * dy).max(0)) as f64).sqrt().round() as i32 };
        let row_w = (w - 2 * inset).max(0) as u32;
        if row_w == 0 {
            continue;
        }
        let _ = canvas.fill_rect(Rect::new(rect.x() + inset, rect.y() + y, row_w, 1));
    }
}

/// A single-pixel rounded outline, matching `fill_rounded_rect`'s corner curve —
/// straight edges via `draw_line`, corners plotted point-by-point per scanline.
pub fn draw_rounded_rect_outline(canvas: &mut Canvas<Window>, rect: Rect, radius: i32, color: Color) {
    let (w, h) = (rect.width() as i32, rect.height() as i32);
    let r = radius.max(0).min(h / 2).min(w / 2);
    canvas.set_draw_color(color);
    if r == 0 || w <= 0 || h <= 0 {
        let _ = canvas.draw_rect(rect);
        return;
    }
    let _ = canvas.draw_line((rect.x() + r, rect.y()), (rect.x() + w - r - 1, rect.y()));
    let _ = canvas.draw_line((rect.x() + r, rect.y() + h - 1), (rect.x() + w - r - 1, rect.y() + h - 1));
    let _ = canvas.draw_line((rect.x(), rect.y() + r), (rect.x(), rect.y() + h - r - 1));
    let _ = canvas.draw_line((rect.x() + w - 1, rect.y() + r), (rect.x() + w - 1, rect.y() + h - r - 1));
    for y in 0..r {
        let dy = r - 1 - y;
        let inset = r - (((r * r - dy * dy).max(0)) as f64).sqrt().round() as i32;
        let _ = canvas.draw_point((rect.x() + inset, rect.y() + y));
        let _ = canvas.draw_point((rect.x() + w - inset - 1, rect.y() + y));
        let _ = canvas.draw_point((rect.x() + inset, rect.y() + h - 1 - y));
        let _ = canvas.draw_point((rect.x() + w - inset - 1, rect.y() + h - 1 - y));
    }
}

// -------------------------------------------------------------------- focus/cards --

/// A slight softening of moonlight-tv's near-square (~2px) tile radius — with no
/// AA/shadow-blur to sell true sharpness at TV viewing distance, a small radius
/// reads cleaner on plain filled rects.
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
/// passes at increasing offset/decreasing alpha approximate a soft glow (no blur
/// primitive available in plain SDL2 2D).
pub fn draw_focus_ring(canvas: &mut Canvas<Window>, rect: Rect, radius: i32) {
    let passes = [(3, 0xff), (6, 0x60)];
    for (offset, alpha) in passes {
        let ring = Rect::new(
            rect.x() - offset,
            rect.y() - offset,
            rect.width() + 2 * offset as u32,
            rect.height() + 2 * offset as u32,
        );
        let color = Color::RGBA(ACCENT_BRIGHT.r, ACCENT_BRIGHT.g, ACCENT_BRIGHT.b, alpha);
        draw_rounded_rect_outline(canvas, ring, radius + offset, color);
    }
}

/// A flat drop-shadow approximation (one soft dark rect offset down-right, no blur
/// available) — matches the reference's shadowed-card look cheaply.
fn draw_card_shadow(canvas: &mut Canvas<Window>, rect: Rect, radius: i32) {
    let shadow = Rect::new(rect.x() + 3, rect.y() + 5, rect.width(), rect.height());
    fill_rounded_rect(canvas, shadow, radius, Color::RGBA(0x00, 0x00, 0x00, 0x60));
}

/// Draws a plain surface card (sidebar rows, settings rows, PIN/IP digit boxes) —
/// shadow, `SIDEBAR_BG` fill, and a focus ring when focused. Returns the (possibly
/// zoom-inflated) rect actually drawn, so callers can center content inside it.
pub fn draw_card(canvas: &mut Canvas<Window>, rect: Rect, focused: bool) -> Rect {
    let r = inflate(rect, focused);
    draw_card_shadow(canvas, r, CARD_RADIUS);
    fill_rounded_rect(canvas, r, CARD_RADIUS, SIDEBAR_BG);
    if focused {
        draw_focus_ring(canvas, r, CARD_RADIUS);
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
    let hash = title.bytes().fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(b as u32));
    POSTER_TINTS[hash as usize % POSTER_TINTS.len()]
}

/// Draws one game/Desktop tile: a tinted placeholder "poster" (no real cover art —
/// the host's management API only returns `{id, title}`) with a large initial
/// letter, plus a bottom title strip — same tile shape moonlight-tv's cover image
/// occupies, filled with a stand-in look instead of a fetched image.
/// Draws one game/Desktop tile. `art`, when `Some` (a decoded cover already turned
/// into a texture by `main.rs` — see `art.rs`), fills the whole card, same as
/// moonlight-tv's cover-image tiles; `None` falls back to a tinted placeholder +
/// initial letter (no real art fetched yet, or the host has none for this title).
/// Either way a bottom title strip overlays the art/tint, matching the reference's
/// always-present (ellipsized) title label.
#[allow(clippy::too_many_arguments)]
pub fn draw_poster_card(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_title: &Font,
    font_value: &Font,
    rect: Rect,
    title: &str,
    art: Option<&sdl2::render::Texture>,
    focused: bool,
) -> Result<()> {
    let r = inflate(rect, focused);
    draw_card_shadow(canvas, r, CARD_RADIUS);

    let strip_h = (font_value.height() + 16).min(r.height() as i32 / 3);
    match art {
        Some(texture) => {
            let _ = canvas.copy(texture, None, r);
        }
        None => {
            fill_rounded_rect(canvas, r, CARD_RADIUS, tint_for(title));
            let initial = title.chars().find(|c| c.is_alphanumeric()).unwrap_or('?').to_uppercase().to_string();
            let (iw, ih) = font_title.size_of(&initial).unwrap_or((0, 0));
            let art_h = r.height() as i32 - strip_h;
            draw_text(
                canvas,
                texture_creator,
                font_title,
                &initial,
                r.x() + (r.width() as i32 - iw as i32) / 2,
                r.y() + (art_h - ih as i32) / 2,
                Color::RGBA(0xff, 0xff, 0xff, 0xa0),
            )?;
        }
    }

    let strip = Rect::new(r.x() + 2, r.y() + r.height() as i32 - strip_h, r.width().saturating_sub(4), strip_h.max(0) as u32);
    fill_rounded_rect(canvas, strip, 0, Color::RGBA(0x00, 0x00, 0x00, 0x70));
    let label = ellipsize(font_value, title, strip.width().saturating_sub(16));
    draw_text(
        canvas,
        texture_creator,
        font_value,
        &label,
        strip.x() + 8,
        strip.y() + (strip.height() as i32 - font_value.height()) / 2,
        WHITE,
    )?;

    if focused {
        draw_focus_ring(canvas, r, CARD_RADIUS);
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
            HostEntry::Known(h) => &h.name,
            HostEntry::Discovered(h) => &h.name,
        }
    }
    pub fn host(&self) -> &str {
        match self {
            HostEntry::Known(h) => &h.host,
            HostEntry::Discovered(h) => &h.addr,
        }
    }
    pub fn port(&self) -> u16 {
        match self {
            HostEntry::Known(h) => h.port,
            HostEntry::Discovered(h) => h.port,
        }
    }
    pub fn is_paired(&self) -> bool {
        matches!(self, HostEntry::Known(h) if h.fingerprint.is_some())
    }
    pub fn mgmt_port(&self) -> Option<u16> {
        match self {
            HostEntry::Known(h) => h.mgmt_port,
            HostEntry::Discovered(h) => h.mgmt_port,
        }
    }
}

/// Draws a simple flat "TV" glyph — a rounded-outline screen + a short stand —
/// for a paired/available sidebar host row.
pub fn draw_tv_icon(canvas: &mut Canvas<Window>, rect: Rect, color: Color) {
    let (w, h) = (rect.width() as i32, rect.height() as i32);
    let screen_h = (h as f32 * 0.72).round() as i32;
    let screen = Rect::new(rect.x(), rect.y(), w as u32, screen_h.max(0) as u32);
    draw_rounded_rect_outline(canvas, screen, 4, color);
    canvas.set_draw_color(color);
    let stand_y = rect.y() + screen_h + (h - screen_h) / 2;
    let _ = canvas.draw_line((rect.x() + w / 2 - w / 6, stand_y), (rect.x() + w / 2 + w / 6, stand_y));
}

/// A padlock glyph (shackle arc approximated by a rounded outline, filled body) —
/// for a not-yet-paired sidebar host row.
pub fn draw_lock_icon(canvas: &mut Canvas<Window>, rect: Rect, color: Color) {
    let (w, h) = (rect.width() as i32, rect.height() as i32);
    let body_h = (h as f32 * 0.58).round() as i32;
    let body = Rect::new(rect.x(), rect.y() + h - body_h, w as u32, body_h.max(0) as u32);
    fill_rounded_rect(canvas, body, 3, color);
    let shackle_w = (w as f32 * 0.55).round() as u32;
    let shackle_h = (h - body_h + 6).max(0) as u32;
    let shackle = Rect::new(rect.x() + (w as u32 - shackle_w) as i32 / 2, rect.y(), shackle_w, shackle_h);
    draw_rounded_rect_outline(canvas, shackle, (shackle_w / 2) as i32, color);
}

/// A "+" glyph for the sidebar's "Add host" row.
pub fn draw_plus_icon(canvas: &mut Canvas<Window>, rect: Rect, color: Color) {
    let (w, h) = (rect.width() as i32, rect.height() as i32);
    let pad = w.min(h) / 4;
    let (cx, cy) = (rect.x() + w / 2, rect.y() + h / 2);
    canvas.set_draw_color(color);
    for off in -1..=1 {
        let _ = canvas.draw_line((rect.x() + pad, cy + off), (rect.x() + w - pad, cy + off));
        let _ = canvas.draw_line((cx + off, rect.y() + pad), (cx + off, rect.y() + h - pad));
    }
}

/// An "X" glyph — modal close buttons.
pub fn draw_close_icon(canvas: &mut Canvas<Window>, rect: Rect, color: Color) {
    let (w, h) = (rect.width() as i32, rect.height() as i32);
    let pad = w.min(h) / 4;
    canvas.set_draw_color(color);
    for off in -1..=1 {
        let _ = canvas.draw_line((rect.x() + pad, rect.y() + pad + off), (rect.x() + w - pad, rect.y() + h - pad + off));
        let _ = canvas.draw_line((rect.x() + w - pad, rect.y() + pad + off), (rect.x() + pad, rect.y() + h - pad + off));
    }
}

/// The gear (settings) icon inscribed in `rect` — vector-drawn, not a font glyph
/// (the system font has no gear/U+2699 glyph). A ring body (via two nested
/// `fill_rounded_rect` circles, the inner one erased in `erase_color`) with a few
/// teeth projected outward, plus a center dot.
pub fn draw_gear_icon(canvas: &mut Canvas<Window>, rect: Rect, color: Color, erase_color: Color) {
    let cx = rect.x() + rect.width() as i32 / 2;
    let cy = rect.y() + rect.height() as i32 / 2;
    let outer_r = (rect.width().min(rect.height()) as i32) / 2 - 2;
    let inner_r = (outer_r as f64 * 0.55).round() as i32;
    let tooth_r = (outer_r as f64 * 1.05).round() as i32;
    let tooth_w = (outer_r as f64 * 0.38).max(3.0).round() as u32;

    const TEETH: usize = 8;
    canvas.set_draw_color(color);
    for i in 0..TEETH {
        let angle = i as f64 * std::f64::consts::TAU / TEETH as f64;
        let tx = cx + (angle.cos() * tooth_r as f64).round() as i32;
        let ty = cy + (angle.sin() * tooth_r as f64).round() as i32;
        let t = Rect::new(tx - tooth_w as i32 / 2, ty - tooth_w as i32 / 2, tooth_w, tooth_w);
        let _ = canvas.fill_rect(t);
    }
    fill_rounded_rect(canvas, Rect::new(cx - outer_r, cy - outer_r, (outer_r * 2) as u32, (outer_r * 2) as u32), outer_r, color);
    fill_rounded_rect(
        canvas,
        Rect::new(cx - inner_r, cy - inner_r, (inner_r * 2) as u32, (inner_r * 2) as u32),
        inner_r,
        erase_color,
    );
    let dot_r = ((inner_r as f64) * 0.4).max(2.0).round() as i32;
    fill_rounded_rect(canvas, Rect::new(cx - dot_r, cy - dot_r, (dot_r * 2) as u32, (dot_r * 2) as u32), dot_r, color);
}

/// Draws the whole sidebar: a flat `SIDEBAR_BG` panel, a "punktfunk" wordmark at
/// the top, one row per host (icon reflects paired/not-paired), then trailing
/// "+ Add host"/"Settings" utility rows. `focused_index` is `Some` only when
/// sidebar itself has focus (see `app.rs`'s `HomeFocus`).
#[allow(clippy::too_many_arguments)]
pub fn draw_sidebar(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_label: &Font,
    font_title: &Font,
    entries: &[HostEntry],
    focused_index: Option<usize>,
    screen_h: u32,
) -> Result<()> {
    fill_rounded_rect(canvas, Rect::new(0, 0, SIDEBAR_W, screen_h), 0, SIDEBAR_BG);
    draw_text(canvas, texture_creator, font_title, "punktfunk", SIDEBAR_PAD, 56, WHITE)?;

    let add_row = entries.len();
    let settings_row = entries.len() + 1;
    for (i, entry) in entries.iter().enumerate() {
        draw_host_row(canvas, texture_creator, font_label, i, entry.name(), entry.is_paired(), focused_index == Some(i))?;
    }
    draw_utility_row(canvas, texture_creator, font_label, add_row, "+ Add host", focused_index == Some(add_row))?;
    draw_utility_row(canvas, texture_creator, font_label, settings_row, "Settings", focused_index == Some(settings_row))?;

    if entries.is_empty() {
        draw_text(
            canvas,
            texture_creator,
            font_label,
            "No hosts yet.",
            SIDEBAR_PAD,
            SIDEBAR_TOP_Y - 32,
            MUTED,
        )?;
    }
    Ok(())
}

fn draw_host_row(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_label: &Font,
    index: usize,
    name: &str,
    paired: bool,
    focused: bool,
) -> Result<()> {
    let rect = sidebar_row_rect(index);
    let drawn = draw_card(canvas, rect, focused);
    let icon_size = 32u32;
    let icon_rect = Rect::new(drawn.x() + 18, drawn.y() + (drawn.height() as i32 - icon_size as i32) / 2, icon_size, icon_size);
    let icon_color = if focused { WHITE } else { MUTED };
    if paired {
        draw_tv_icon(canvas, icon_rect, icon_color);
    } else {
        draw_lock_icon(canvas, icon_rect, icon_color);
    }
    draw_text(
        canvas,
        texture_creator,
        font_label,
        name,
        drawn.x() + 18 + icon_size as i32 + 16,
        drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
        if focused { WHITE } else { MUTED },
    )?;
    Ok(())
}

fn draw_utility_row(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_label: &Font,
    index: usize,
    label: &str,
    focused: bool,
) -> Result<()> {
    let rect = sidebar_row_rect(index);
    let drawn = draw_card(canvas, rect, focused);
    let icon_size = 28u32;
    let icon_rect = Rect::new(drawn.x() + 20, drawn.y() + (drawn.height() as i32 - icon_size as i32) / 2, icon_size, icon_size);
    let icon_color = if focused { WHITE } else { MUTED };
    if label.starts_with('+') {
        draw_plus_icon(canvas, icon_rect, icon_color);
    } else {
        draw_gear_icon(canvas, icon_rect, icon_color, drawn_bg_for(focused));
    }
    draw_text(
        canvas,
        texture_creator,
        font_label,
        label.trim_start_matches('+').trim(),
        drawn.x() + 20 + icon_size as i32 + 16,
        drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
        if focused { WHITE } else { MUTED },
    )?;
    Ok(())
}

fn drawn_bg_for(focused: bool) -> Color {
    // The gear icon punches an "erase" hole for its inner circle — must match
    // whatever's actually beneath it (the card's own fill, which doesn't change
    // with focus — only the ring/zoom do — so this is always SIDEBAR_BG).
    let _ = focused;
    SIDEBAR_BG
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

pub fn hit_test_grid_card(mouse_x: i32, mouse_y: i32, columns: usize, count: usize, grid_x: i32, available_w: u32) -> Option<usize> {
    if mouse_x < grid_x {
        return None;
    }
    (0..count).find(|&i| grid_card_rect(i, columns, grid_x, available_w).contains_point((mouse_x, mouse_y)))
}

// ----------------------------------------------------------------------- modals --

/// Dims the already-rendered frame beneath a modal (Settings/Pairing/Add host all
/// render on top of the current Home frame, then this, then their own card).
pub fn draw_modal_backdrop(canvas: &mut Canvas<Window>, screen_w: u32, screen_h: u32) {
    canvas.set_draw_color(MODAL_SCRIM);
    let _ = canvas.fill_rect(Rect::new(0, 0, screen_w, screen_h));
}

/// A centered glass card of `(width_frac * screen_w, height)`.
pub fn modal_card_rect(screen_w: u32, screen_h: u32, width_frac: f32, height: u32) -> Rect {
    let w = (screen_w as f32 * width_frac).round() as u32;
    let x = (screen_w as i32 - w as i32) / 2;
    let y = (screen_h as i32 - height as i32) / 2;
    Rect::new(x, y, w, height)
}

pub fn draw_modal_card(canvas: &mut Canvas<Window>, rect: Rect) {
    draw_card_shadow(canvas, rect, MODAL_RADIUS);
    fill_rounded_rect(canvas, rect, MODAL_RADIUS, SIDEBAR_BG);
    draw_rounded_rect_outline(canvas, rect, MODAL_RADIUS, Color::RGBA(0xff, 0xff, 0xff, 0x18));
}

/// The modal close (X) button rect, top-right inset of `card_rect`.
pub fn modal_close_rect(card_rect: Rect) -> Rect {
    const SIZE: u32 = 44;
    const MARGIN: i32 = 20;
    Rect::new(card_rect.x() + card_rect.width() as i32 - MARGIN - SIZE as i32, card_rect.y() + MARGIN, SIZE, SIZE)
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
    let next = if forward { (idx + 1) % len } else { (idx + len - 1) % len };
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
        .map(|(_, _, s)| s.to_string())
        .unwrap_or_else(|| format!("{width}x{height}"))
}

pub fn settings_rows(settings: &Settings) -> Vec<SettingsRow> {
    let bitrate_frac = (settings.bitrate_kbps.saturating_sub(BITRATE_MIN_KBPS)) as f32
        / (BITRATE_MAX_KBPS - BITRATE_MIN_KBPS) as f32;
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
            value: if settings.hdr_enabled { "On".into() } else { "Off".into() },
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
        ROW_FRAMERATE => REFRESH_RATES.iter().position(|hz| *hz == settings.refresh_hz).unwrap_or(0),
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
            let delta = BITRATE_STEP_KBPS as i64 * if forward { 1 } else { -1 };
            let next = (settings.bitrate_kbps as i64 + delta).clamp(BITRATE_MIN_KBPS as i64, BITRATE_MAX_KBPS as i64);
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
pub fn draw_settings_rows(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_label: &Font,
    font_value: &Font,
    rows: &[SettingsRow],
    focused_index: usize,
    content_rect: Rect,
) -> Result<()> {
    for (i, row) in rows.iter().enumerate() {
        let y = content_rect.y() + i as i32 * (SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP);
        let focused = i == focused_index;
        let row_rect = Rect::new(content_rect.x(), y, content_rect.width(), SETTINGS_ROW_H);
        let drawn = draw_card(canvas, row_rect, focused);

        let icon_pad = 24;
        let icon_rect = Rect::new(
            drawn.x() + icon_pad,
            drawn.y() + (drawn.height() as i32 - SETTINGS_ICON_SIZE as i32) / 2,
            SETTINGS_ICON_SIZE,
            SETTINGS_ICON_SIZE,
        );
        let icon_color = if focused { WHITE } else { MUTED };
        match row.kind {
            RowKind::Dropdown if i == ROW_RESOLUTION => draw_monitor_icon(canvas, icon_rect, icon_color),
            RowKind::Dropdown => draw_clock_icon(canvas, icon_rect, icon_color),
            RowKind::Slider => draw_bars_icon(canvas, icon_rect, icon_color),
            RowKind::Toggle => draw_sun_icon(canvas, icon_rect, icon_color),
        }
        let label_x = icon_rect.x() + SETTINGS_ICON_SIZE as i32 + 20;
        draw_text(
            canvas,
            texture_creator,
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
                draw_dropdown_pill(canvas, texture_creator, font_value, pill, &row.value, focused)?;
            }
            RowKind::Slider => {
                let value_w = font_value.size_of(&row.value).map(|(w, _)| w).unwrap_or(0);
                let track_w = 220u32.min(drawn.width() / 3);
                let value_x = drawn.x() + drawn.width() as i32 - control_pad - value_w as i32;
                draw_text(
                    canvas,
                    texture_creator,
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
                draw_slider_with_thumb(canvas, track, row.fraction, focused);
            }
            RowKind::Toggle => {
                let switch = Rect::new(drawn.x() + drawn.width() as i32 - control_pad - 64, drawn.y() + (drawn.height() as i32 - 34) / 2, 64, 34);
                draw_switch(canvas, switch, row.value == "On");
            }
        }
    }
    Ok(())
}

/// A rounded pill button showing the current dropdown value + a small caret.
pub fn draw_dropdown_pill(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font: &Font,
    rect: Rect,
    label: &str,
    focused: bool,
) -> Result<()> {
    let radius = rect.height() as i32 / 2;
    fill_rounded_rect(canvas, rect, radius, Color::RGBA(0xff, 0xff, 0xff, 0x12));
    draw_rounded_rect_outline(canvas, rect, radius, if focused { ACCENT_BRIGHT } else { Color::RGBA(0xff, 0xff, 0xff, 0x30) });
    let text = format!("{label}  ▾");
    let text_w = font.size_of(&text).map(|(w, _)| w).unwrap_or(0);
    draw_text(
        canvas,
        texture_creator,
        font,
        &text,
        rect.x() + (rect.width() as i32 - text_w as i32) / 2,
        rect.y() + (rect.height() as i32 - font.height()) / 2,
        WHITE,
    )?;
    Ok(())
}

/// A round-thumbed slider track, shadowed knob (matches the reference's
/// slider-knob-shadow theme touch).
pub fn draw_slider_with_thumb(canvas: &mut Canvas<Window>, rect: Rect, fraction: f32, focused: bool) {
    let track_h = rect.height();
    let track = Rect::new(rect.x(), rect.y(), rect.width(), track_h);
    fill_rounded_rect(canvas, track, track_h as i32 / 2, Color::RGBA(0xff, 0xff, 0xff, 0x22));
    let filled_w = (rect.width() as f32 * fraction.clamp(0.0, 1.0)) as u32;
    if filled_w > 0 {
        let filled = Rect::new(rect.x(), rect.y(), filled_w.max(track_h), track_h);
        fill_rounded_rect(canvas, filled, track_h as i32 / 2, ACCENT);
    }
    let thumb_r = 14i32;
    let cx = rect.x() + filled_w as i32;
    let cy = rect.y() + rect.height() as i32 / 2;
    fill_rounded_rect(
        canvas,
        Rect::new(cx - thumb_r + 2, cy - thumb_r + 3, (thumb_r * 2) as u32, (thumb_r * 2) as u32),
        thumb_r,
        Color::RGBA(0x00, 0x00, 0x00, 0x50),
    );
    fill_rounded_rect(
        canvas,
        Rect::new(cx - thumb_r, cy - thumb_r, (thumb_r * 2) as u32, (thumb_r * 2) as u32),
        thumb_r,
        if focused { WHITE } else { MUTED },
    );
}

/// A modern sliding pill switch (iOS/Android-style) — accent-filled track with
/// the knob at the right when on, muted track with the knob at the left when
/// off. Replaces the old checkbox for a more contemporary boolean control.
pub fn draw_switch(canvas: &mut Canvas<Window>, rect: Rect, on: bool) {
    let radius = rect.height() as i32 / 2;
    fill_rounded_rect(canvas, rect, radius, if on { ACCENT } else { Color::RGBA(0xff, 0xff, 0xff, 0x22) });
    let knob_r = radius - 4;
    let cy = rect.y() + rect.height() as i32 / 2;
    let cx = if on { rect.x() + rect.width() as i32 - radius } else { rect.x() + radius };
    fill_rounded_rect(
        canvas,
        Rect::new(cx - knob_r + 1, cy - knob_r + 2, (knob_r * 2) as u32, (knob_r * 2) as u32),
        knob_r,
        Color::RGBA(0x00, 0x00, 0x00, 0x40),
    );
    fill_rounded_rect(canvas, Rect::new(cx - knob_r, cy - knob_r, (knob_r * 2) as u32, (knob_r * 2) as u32), knob_r, WHITE);
    Ok(())
}

/// Renders a dropdown's options as an overlay list anchored just below the row that
/// opened it, inside the settings modal card.
pub fn draw_dropdown_overlay(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_value: &Font,
    options: &[String],
    focused_index: usize,
    rect: Rect,
) -> Result<()> {
    let row_h = 56u32;
    let gap = 6i32;
    let overlay_h = options.len() as i32 * (row_h as i32 + gap);
    let bg_rect = Rect::new(rect.x(), rect.y(), rect.width(), overlay_h.max(0) as u32);
    fill_rounded_rect(canvas, bg_rect, CARD_RADIUS, Color::RGBA(0x10, 0x10, 0x10, 0xf0));
    draw_rounded_rect_outline(canvas, bg_rect, CARD_RADIUS, Color::RGBA(0xff, 0xff, 0xff, 0x20));
    for (i, opt) in options.iter().enumerate() {
        let y = rect.y() + i as i32 * (row_h as i32 + gap);
        let row_rect = Rect::new(rect.x(), y, rect.width(), row_h);
        let focused = i == focused_index;
        let drawn = draw_card(canvas, row_rect, focused);
        draw_text(
            canvas,
            texture_creator,
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
}

impl Default for AddHostState {
    fn default() -> Self {
        // Prefills punktfunk's conventional default port (9777 — see
        // `store::dev_override_connect`'s fallback) so the user only has to dial in
        // the IP address.
        AddHostState { digits: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9, 7, 7, 7], index: 0 }
    }
}

impl AddHostState {
    fn octet(&self, i: usize) -> u8 {
        let d = &self.digits[i * 3..i * 3 + 3];
        (d[0] as u32 * 100 + d[1] as u32 * 10 + d[2] as u32).min(255) as u8
    }

    fn port_value(&self) -> u16 {
        let v = self.digits[12..17].iter().fold(0u32, |acc, &digit| acc * 10 + digit as u32);
        v.min(u16::MAX as u32) as u16
    }

    pub fn host_and_port(&self) -> (String, u16) {
        (
            format!("{}.{}.{}.{}", self.octet(0), self.octet(1), self.octet(2), self.octet(3)),
            self.port_value(),
        )
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

    pub fn display_text(&self) -> String {
        format!(
            "{}{}{}.{}{}{}.{}{}{}.{}{}{}:{}{}{}{}{}",
            self.digits[0],
            self.digits[1],
            self.digits[2],
            self.digits[3],
            self.digits[4],
            self.digits[5],
            self.digits[6],
            self.digits[7],
            self.digits[8],
            self.digits[9],
            self.digits[10],
            self.digits[11],
            self.digits[12],
            self.digits[13],
            self.digits[14],
            self.digits[15],
            self.digits[16],
        )
    }
}

/// Draws `text` left-aligned at `(x, y)`, rendering the character at `focus_char` in
/// `focus_color` and every other character in `base_color` — used by the add-host
/// screen to show which digit Left/Right/number-keys currently edit.
#[allow(clippy::too_many_arguments)]
pub fn draw_highlighted_text(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
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
        let w = draw_text(canvas, texture_creator, font, &s, cursor_x, y, color)?;
        cursor_x += w as i32;
    }
    Ok(())
}
