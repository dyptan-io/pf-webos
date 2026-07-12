//! Drawing/input-mapping primitives shared by the pre-stream screens (host list, PIN
//! pairing, settings, game library, add-host), rendered with plain SDL2 2D primitives
//! (rects + `SDL2_ttf` text — no Skia/Vulkan available on webOS). Colors and spacing
//! proportions started from `crates/pf-console-ui`'s gamepad-driven console UI so
//! this didn't invent a look from scratch, but rendered flat with rounded rects (no
//! glass blur/spring easing) rather than attempting pixel parity.
use anyhow::{Context, Result};
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, TextureCreator};
use sdl2::ttf::Font;
use sdl2::video::{Window, WindowContext};

use crate::discovery::DiscoveredHost;
use crate::store::{KnownHost, Settings};

/// `pf-console-ui`'s BRAND accent (`#8578f5`).
pub const BRAND: Color = Color::RGB(0x85, 0x78, 0xf5);
pub const WHITE: Color = Color::RGB(0xff, 0xff, 0xff);
pub const DIM: Color = Color::RGBA(0xff, 0xff, 0xff, 0x8c);
pub const ERROR_RED: Color = Color::RGB(0xff, 0x92, 0x89);
/// `pf-console-ui`'s dark glass panel base (`#15151f`), flattened (no blur).
pub const PANEL_BG: Color = Color::RGBA(0x15, 0x15, 0x1f, 0xc0);
pub const PANEL_BG_FOCUSED: Color = Color::RGBA(0x2a, 0x26, 0x4a, 0xe0);
/// The calm indigo form backdrop `pf-console-ui` uses behind pairing/settings.
pub const FORM_BG: Color = Color::RGB(0x13, 0x0f, 0x26);
/// Paired with `FORM_BG` for a subtle top-to-bottom background gradient (see
/// `fill_vertical_gradient`) — a touch lighter/more purple than the flat bottom tone.
pub const FORM_BG_TOP: Color = Color::RGB(0x1d, 0x17, 0x38);

/// LG's own system UI font — already on-device, no bundling needed (see Cargo.toml).
pub const SYSTEM_FONT_PATH: &str = "/usr/share/fonts/LG_Smart_UI-Regular.ttf";

/// How a settings row behaves when focused/confirmed — drives both rendering and
/// `app.rs`'s event handling.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A plain confirm-only row (host entries, "+ Add host", game titles).
    Action,
    /// Confirm opens an inline list of options (Resolution, Frame rate).
    Dropdown,
    /// Left/Right adjust a value in a range, rendered as a filled bar (Bitrate).
    Slider,
    /// Confirm/Left/Right flip a boolean (HDR).
    Toggle,
}

/// One focusable row in a list screen (host list or settings).
pub struct Row {
    pub label: String,
    pub value: String,
    pub kind: RowKind,
    /// 0.0-1.0 fill fraction, only meaningful for `RowKind::Slider`.
    pub fraction: f32,
}

impl Row {
    pub fn action(label: impl Into<String>, value: impl Into<String>) -> Row {
        Row { label: label.into(), value: value.into(), kind: RowKind::Action, fraction: 0.0 }
    }
}

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
    /// "Forget this host" on the host-list screen — deliberately a separate key
    /// from Back/Confirm so it can't be hit by accident (see `app.rs`).
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

/// Loads the system font at a size proportional to the display height (design
/// reference: `pf-console-ui`'s 16 design-unit row label size at a 720px-tall
/// reference screen — `size = 16 * height / 720`).
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

/// Default corner radius for row/card panels — TV UIs lean on soft rounding rather
/// than sharp rects; scaled the same way font sizes are (see `load_font`).
pub const PANEL_RADIUS: i32 = 16;

/// Fills a rounded rectangle one scanline at a time (a plain filled rect in the
/// middle, an inset computed per-row from the corner circle's equation near the
/// top/bottom edges) — no SDL2_gfx dependency, cheap enough for the handful of
/// panels on screen at once.
pub fn fill_rounded_rect(canvas: &mut Canvas<Window>, rect: Rect, radius: i32, color: Color) {
    let (w, h) = (rect.width() as i32, rect.height() as i32);
    let r = radius.max(0).min(h / 2).min(w / 2);
    canvas.set_draw_color(color);
    if r == 0 {
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
    if r == 0 {
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

/// Fills the whole canvas with a subtle top-to-bottom gradient between two colors —
/// a flat `FORM_BG` fill read as dated on a TV; a soft gradient is a cheap way to
/// add depth without any blur/shader work. `bands` trades smoothness for draw calls
/// (32 is imperceptible banding at TV viewing distance, negligible cost).
pub fn fill_vertical_gradient(canvas: &mut Canvas<Window>, screen_w: u32, screen_h: u32, top: Color, bottom: Color) {
    const BANDS: i32 = 32;
    let band_h = (screen_h as i32 / BANDS).max(1);
    for i in 0..BANDS {
        let t = i as f64 / (BANDS - 1) as f64;
        let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
        canvas.set_draw_color(Color::RGB(lerp(top.r, bottom.r), lerp(top.g, bottom.g), lerp(top.b, bottom.b)));
        let y = i * band_h;
        let h = if i == BANDS - 1 { (screen_h as i32 - y).max(band_h) } else { band_h };
        let _ = canvas.fill_rect(Rect::new(0, y, screen_w, h as u32));
    }
}

/// Width of the accent bar drawn on the left edge of a focused row — a common
/// modern TV-UI focus cue (Netflix/YouTube-style), used instead of a full-rect
/// border so focus reads as a clean highlight rather than a boxed-in outline.
const FOCUS_ACCENT_W: i32 = 6;

/// Draws one row's panel background: a rounded, softly-bordered card, with a
/// left accent bar in `BRAND` when focused.
pub fn draw_row_panel(canvas: &mut Canvas<Window>, rect: Rect, focused: bool) {
    fill_rounded_rect(canvas, rect, PANEL_RADIUS, if focused { PANEL_BG_FOCUSED } else { PANEL_BG });
    draw_rounded_rect_outline(
        canvas,
        rect,
        PANEL_RADIUS,
        if focused { Color::RGBA(BRAND.r, BRAND.g, BRAND.b, 0xd0) } else { Color::RGBA(0xff, 0xff, 0xff, 0x18) },
    );
    if focused {
        let accent = Rect::new(
            rect.x() + PANEL_RADIUS / 3,
            rect.y() + PANEL_RADIUS,
            FOCUS_ACCENT_W as u32,
            rect.height().saturating_sub(2 * PANEL_RADIUS as u32),
        );
        fill_rounded_rect(canvas, accent, FOCUS_ACCENT_W / 2, BRAND);
    }
}

/// Renders a vertical list of rows starting at `top_y`, each `row_h` tall with
/// `gap` between — the shared layout `pf-console-ui` uses for both its host
/// carousel-as-list and its settings rows.
#[allow(clippy::too_many_arguments)]
pub fn draw_rows(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_label: &Font,
    font_value: &Font,
    rows: &[Row],
    focused_index: usize,
    left: i32,
    top_y: i32,
    width: u32,
    row_h: u32,
    gap: i32,
) -> Result<()> {
    for (i, row) in rows.iter().enumerate() {
        let y = top_y + i as i32 * (row_h as i32 + gap);
        let focused = i == focused_index;
        let rect = Rect::new(left, y, width, row_h);
        draw_row_panel(canvas, rect, focused);
        let pad = 28;
        draw_text(
            canvas,
            texture_creator,
            font_label,
            &row.label,
            left + pad,
            y + (row_h as i32 - font_label.height()) / 2,
            WHITE,
        )?;
        let value_color = if focused { WHITE } else { DIM };
        if row.kind == RowKind::Slider {
            draw_slider(canvas, rect, row.fraction, focused);
            let value_w = font_value.size_of(&row.value).map(|(w, _)| w).unwrap_or(0);
            draw_text(
                canvas,
                texture_creator,
                font_value,
                &row.value,
                left + width as i32 - pad - value_w as i32,
                y + pad / 2 - font_value.height() / 2,
                value_color,
            )?;
            continue;
        }
        let mut value = row.value.clone();
        if row.kind == RowKind::Dropdown {
            // Plain ASCII, not a unicode glyph — the system font's missing U+2699
            // (gear) glyph is exactly what made the old in-list Settings row render
            // as a broken box (see `app.rs`'s `host_rows` docs).
            value.push_str("  >");
        }
        let value_w = font_value.size_of(&value).map(|(w, _)| w).unwrap_or(0);
        draw_text(
            canvas,
            texture_creator,
            font_value,
            &value,
            left + width as i32 - pad - value_w as i32,
            y + (row_h as i32 - font_value.height()) / 2,
            value_color,
        )?;
    }
    Ok(())
}

/// Draws a horizontal filled-bar slider track anchored to the bottom of `row_rect`.
fn draw_slider(canvas: &mut Canvas<Window>, row_rect: Rect, fraction: f32, focused: bool) {
    let pad = 28;
    let track_h = 10u32;
    let track = Rect::new(
        row_rect.x() + pad,
        row_rect.y() + row_rect.height() as i32 - pad + 4,
        row_rect.width().saturating_sub(2 * pad as u32),
        track_h,
    );
    fill_rounded_rect(canvas, track, track_h as i32 / 2, Color::RGBA(0xff, 0xff, 0xff, 0x22));
    let filled_w = (track.width() as f32 * fraction.clamp(0.0, 1.0)) as u32;
    if filled_w > 0 {
        // At least pill-height wide so the rounded cap doesn't look clipped when
        // the slider is near its minimum.
        let filled = Rect::new(track.x(), track.y(), filled_w.max(track_h), track_h);
        let color = if focused { BRAND } else { Color::RGBA(BRAND.r, BRAND.g, BRAND.b, 0xa0) };
        fill_rounded_rect(canvas, filled, track_h as i32 / 2, color);
    }
}

/// Renders a dropdown's options as an overlay list anchored just below the row that
/// opened it. Returns the overlay's bottom `y` (unused by callers today, but keeps the
/// signature consistent with `draw_rows`).
#[allow(clippy::too_many_arguments)]
pub fn draw_dropdown_overlay(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_value: &Font,
    options: &[String],
    focused_index: usize,
    left: i32,
    top_y: i32,
    width: u32,
) -> Result<()> {
    let row_h = 64u32;
    let gap = 8i32;
    let overlay_h = options.len() as i32 * (row_h as i32 + gap);
    let bg_rect = Rect::new(left, top_y, width, overlay_h.max(0) as u32);
    fill_rounded_rect(canvas, bg_rect, PANEL_RADIUS, Color::RGBA(0x0a, 0x08, 0x18, 0xf0));
    draw_rounded_rect_outline(canvas, bg_rect, PANEL_RADIUS, Color::RGBA(BRAND.r, BRAND.g, BRAND.b, 0xb0));
    for (i, opt) in options.iter().enumerate() {
        let y = top_y + i as i32 * (row_h as i32 + gap);
        let rect = Rect::new(left, y, width, row_h);
        let focused = i == focused_index;
        draw_row_panel(canvas, rect, focused);
        draw_text(
            canvas,
            texture_creator,
            font_value,
            opt,
            left + 28,
            y + (row_h as i32 - font_value.height()) / 2,
            if focused { WHITE } else { DIM },
        )?;
    }
    Ok(())
}

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

/// The header's Settings button rect (icon drawn separately via `draw_gear_icon`) —
/// deliberately separate from the host-list's row list, not mixed into it (it used
/// to be a synthetic trailing row there, both the wrong place for it and rendered
/// with a glyph the system font lacks).
pub fn settings_button_rect(screen_w: u32) -> Rect {
    const W: u32 = 72;
    const H: u32 = 64;
    const MARGIN: i32 = 48;
    Rect::new(screen_w as i32 - MARGIN - W as i32, MARGIN, W, H)
}

/// Draws a simple flat "gear" (settings) icon inscribed in `rect` — vector-drawn,
/// not a font glyph (the system font has no gear/U+2699 glyph — see `host_rows`'s
/// docs in `app.rs` on why a text label was tried and rejected there too). A ring
/// body (via two nested `fill_rounded_rect` circles, the inner one erased in the
/// button's own panel color) with a few teeth projected outward, plus a center dot.
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

/// The persistent top-left "Back" button on every non-root screen (Settings,
/// Library, Add host, Pairing) — a fixed, always-in-the-same-place corner
/// affordance instead of a row mixed into the list, mirroring how the Settings
/// button lives in the header rather than inside the host list.
pub fn back_button_rect() -> Rect {
    Rect::new(48, 48, 140, 64)
}

pub fn draw_back_button(
    canvas: &mut Canvas<Window>,
    texture_creator: &TextureCreator<WindowContext>,
    font_value: &Font,
    focused: bool,
) -> Result<()> {
    let rect = back_button_rect();
    draw_row_panel(canvas, rect, focused);
    let label = "< Back";
    let label_w = font_value.size_of(label).map(|(w, _)| w).unwrap_or(0);
    draw_text(
        canvas,
        texture_creator,
        font_value,
        label,
        rect.x() + (rect.width() as i32 - label_w as i32) / 2,
        rect.y() + (rect.height() as i32 - font_value.height()) / 2,
        if focused { WHITE } else { DIM },
    )?;
    Ok(())
}

/// Whether `(x, y)` falls within the top-left Back button — shared by every
/// screen's mouse handling.
pub fn hit_test_back_button(x: i32, y: i32) -> bool {
    back_button_rect().contains_point((x, y))
}

/// One entry in the host list — either a fully known/paired host or a freshly
/// discovered (not yet paired) one.
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

// Sized for a 10-foot TV viewing distance, not a desktop/phone screen — noticeably
// bigger than a typical settings-list row.
pub const ROW_H: u32 = 96;
pub const ROW_GAP: i32 = 14;
pub const ROW_MAX_W: u32 = 960;
/// Where the row list starts vertically — shared by `app.rs`'s render calls (host
/// list and settings both start here) and `hit_test_row` below, so the Magic
/// Remote's pointer mode hovers the same rows it visually sees.
pub const ROWS_TOP_Y: i32 = 190;

/// Maps a Magic Remote pointer position to the row it's hovering, using the exact
/// same geometry `draw_rows` renders with. `None` outside every row's rect.
pub fn hit_test_row(mouse_x: i32, mouse_y: i32, screen_w: u32, row_count: usize) -> Option<usize> {
    let left = ((screen_w.saturating_sub(ROW_MAX_W)) / 2) as i32;
    let width = ROW_MAX_W.min(screen_w.saturating_sub(64));
    if mouse_x < left || mouse_x > left + width as i32 {
        return None;
    }
    for i in 0..row_count {
        let y = ROWS_TOP_Y + i as i32 * (ROW_H as i32 + ROW_GAP);
        if mouse_y >= y && mouse_y < y + ROW_H as i32 {
            return Some(i);
        }
    }
    None
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

/// Bitrate slider range/step, in kbps — the user's explicit ask ("10-150 Mbps max"),
/// replacing the old discrete preset list (which included a 0="Automatic" option that
/// doesn't fit a continuous range).
pub const BITRATE_MIN_KBPS: u32 = 10_000;
pub const BITRATE_MAX_KBPS: u32 = 150_000;
pub const BITRATE_STEP_KBPS: u32 = 5_000;

/// Settings-screen row indices — shared by `settings_rows`, `adjust_setting`, and
/// `app.rs`'s event handling so the mapping only lives in one place. Back is now a
/// persistent top-left button (`back_button_rect`), not a row in this list — `App`
/// treats it as its own nav stop (index 0) above these, same pattern as the
/// HostList screen's header Settings button.
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

/// Steps a zero-based index by ±1 with wraparound over `len` entries.
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

/// Builds the settings screen's row labels/values from the current `Settings` —
/// Resolution/Frame rate/Bitrate/HDR only; Back is the persistent top-left button
/// (`back_button_rect`), not a row here (see `ROW_*` docs).
pub fn settings_rows(settings: &Settings) -> Vec<Row> {
    let bitrate_frac = (settings.bitrate_kbps.saturating_sub(BITRATE_MIN_KBPS)) as f32
        / (BITRATE_MAX_KBPS - BITRATE_MIN_KBPS) as f32;
    vec![
        Row {
            label: "Resolution".into(),
            value: resolution_label(settings.width, settings.height),
            kind: RowKind::Dropdown,
            fraction: 0.0,
        },
        Row {
            label: "Frame rate".into(),
            value: format!("{} Hz", settings.refresh_hz),
            kind: RowKind::Dropdown,
            fraction: 0.0,
        },
        Row {
            label: "Bitrate".into(),
            value: format!("{} Mbps", settings.bitrate_kbps / 1000),
            kind: RowKind::Slider,
            fraction: bitrate_frac,
        },
        Row {
            label: "HDR".into(),
            value: if settings.hdr_enabled { "On".into() } else { "Off".into() },
            kind: RowKind::Toggle,
            fraction: 0.0,
        },
    ]
}

/// The option labels for a dropdown row (Resolution/Frame rate only — other rows
/// aren't dropdowns and return an empty list).
pub fn dropdown_options(row_index: usize) -> Vec<String> {
    match row_index {
        ROW_RESOLUTION => RESOLUTIONS.iter().map(|(w, h, _)| resolution_label(*w, *h)).collect(),
        ROW_FRAMERATE => REFRESH_RATES.iter().map(|hz| format!("{hz} Hz")).collect(),
        _ => Vec::new(),
    }
}

/// Which option index in `dropdown_options(row_index)` matches the current setting —
/// used to pre-focus the overlay when it opens.
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

/// Applies a chosen dropdown option index to `settings`. No-op for non-dropdown rows.
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

/// Applies a left/right adjustment to `settings` for the given settings-row index
/// (see `settings_rows`/the `ROW_*` constants for the matching order). Returns `true`
/// if it changed. Resolution/Frame rate also support direct left/right cycling as a
/// shortcut alongside their dropdown (opened via Confirm in `app.rs`).
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
            let next = (settings.bitrate_kbps as i64 + delta)
                .clamp(BITRATE_MIN_KBPS as i64, BITRATE_MAX_KBPS as i64);
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
