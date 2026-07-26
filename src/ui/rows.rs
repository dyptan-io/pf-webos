//! The generic focusable-row list: `FocusRow`/`RowKind` plus every control
//! (dropdown pill, slider, switch, confirm button) a row can carry.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::ttf::Font;

/// How a focus row's right-hand control behaves — shared by the settings
/// modal's row list and the Wake modal's two rows (`draw_focus_rows`'s single
/// implementation, see its docs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Dropdown,
    Slider,
    Toggle,
    /// A plain actionable row — icon + label, with `value` (if any) as a muted hint on
    /// the right and no control at all. This is what makes a screen out of nothing but
    /// a list: see [`ListModal`](crate::ui::ListModal), which the host-actions menu and
    /// the Settings sub-page links are both built from. Confirm on the row *is* the
    /// action; there is nothing to adjust in place.
    Action,
}

/// One focusable icon + label (+ dropdown pill / slider / switch) row, drawn
/// by `draw_focus_rows`/`draw_focus_row` — shared by the settings modal's row
/// list (`settings_rows`) and the Wake modal's toggle row (`wake_rows`).
pub struct FocusRow {
    pub icon: &'static str,
    pub label: String,
    pub value: String,
    pub kind: RowKind,
    /// 0.0-1.0 fill fraction, only meaningful for `RowKind::Slider`.
    pub fraction: f32,
    /// Destructive action (Forget host) — drawn in `ERROR_RED` rather than the
    /// normal muted/white pair, so it reads as dangerous before it's confirmed.
    pub danger: bool,
}

impl FocusRow {
    /// A plain [`RowKind::Action`] row — the common case for list-modal screens.
    pub fn action(icon: &'static str, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            value: String::new(),
            kind: RowKind::Action,
            fraction: 0.0,
            danger: false,
        }
    }

    /// Same, with a muted right-hand hint (e.g. a host's address under its name).
    pub fn action_with_value(icon: &'static str, label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            ..Self::action(icon, label)
        }
    }

    /// Marks this row destructive (see [`FocusRow::danger`]).
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

// Generous, TV-scale rows — each is its own focusable card (icon + label left,
// control right), consistent with the sidebar/grid's card+focus-ring language
// rather than the bare flat rows the upstream reference uses.
pub const SETTINGS_ROW_H: u32 = 92;
pub const SETTINGS_ROW_GAP: i32 = 8;
pub const SETTINGS_ICON_SIZE: u32 = 30;

/// Row `index`'s rect within a modal's `content_rect` (the modal card's
/// interior, below its title/divider) — the one place this stacked-row layout
/// formula lives, shared by `draw_focus_rows` and `app.rs`'s `draw_list`
/// (which needs it to position the composited focused-row tile), for both the
/// settings modal's row list and the Wake modal's two rows.
pub fn focus_row_rect(content_rect: Rect, index: usize) -> Rect {
    let y = content_rect.y() + index as i32 * (SETTINGS_ROW_H as i32 + SETTINGS_ROW_GAP);
    Rect::new(content_rect.x(), y, content_rect.width(), SETTINGS_ROW_H)
}

/// Fixed reserved width for a slider row's value label (e.g. "150 Mbps",
/// "Automatic") — the track's position is anchored to this fixed slot rather
/// than to the label's actual (variable) text width, so the track never
/// shifts or appears to resize as the label's digit count changes.
pub const SLIDER_VALUE_SLOT_W: i32 = 150;

/// Draws a modal's focus-row list inside `content_rect` — icon + label on the
/// left, a dropdown pill / slider / modern switch on the right — shared by the
/// settings modal (`settings_rows`) and the Wake modal (`wake_rows`). Only the
/// focused row gets a background card (see [`draw_selectable`]); an unfocused
/// row is bare. Every row here renders at its normal, un-zoomed size — the
/// focused row's zoom-in is a GPU animation applied on top (`app.rs`'s
/// `draw_list`), not baked into this rasterized layer.
pub fn draw_focus_rows(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    rows: &[FocusRow],
    focused_index: usize,
    open_dropdown_row: Option<usize>,
    content_rect: Rect,
) -> Result<()> {
    for (i, row) in rows.iter().enumerate() {
        let row_rect = focus_row_rect(content_rect, i);
        let switch_frac = if row.value == "On" { 1.0 } else { 0.0 };
        draw_focus_row(
            painter,
            text_cache,
            fonts,
            row,
            i == focused_index,
            open_dropdown_row == Some(i),
            switch_frac,
            row_rect,
        )?;
    }
    Ok(())
}

/// A modal's focused row, as its own padded transparent tile — composited by
/// the GPU over its shell (which draws every row unfocused via
/// `draw_focus_rows`, see its docs). Mirrors `render_focused_row_tile`'s
/// sidebar equivalent: moving row focus recomposites this small tile instead of
/// re-rasterizing the whole modal. `switch_frac` (see `draw_switch`) lets the
/// caller animate a `Toggle` row's knob slide independently of everything else
/// on the row.
pub fn render_focus_row_tile(
    text_cache: &mut TextCache,
    fonts: &Fonts,
    rows: &[FocusRow],
    content_width: u32,
    index: usize,
    dropdown_open: bool,
    switch_frac: f32,
) -> Result<Painter> {
    let pad = ROW_TILE_PAD;
    let rect = Rect::new(pad, pad, content_width, SETTINGS_ROW_H);
    let mut p = Painter::new(content_width + 2 * pad as u32, SETTINGS_ROW_H + 2 * pad as u32);
    if let Some(row) = rows.get(index) {
        draw_focus_row(&mut p, text_cache, fonts, row, true, dropdown_open, switch_frac, rect)?;
    }
    Ok(p)
}

/// Every row unfocused, as one tile at its own full (unscrolled) height — the
/// Settings modal's `Tile::ScrollContent(Screen::Settings)`. Scrolling crops/
/// repositions this via a GPU-side `DrawCmd::TexCropped` instead of
/// re-rasterizing, so this only needs rebuilding when a value or the open
/// dropdown changes, never on scroll.
pub fn render_focus_rows_tile(
    text_cache: &mut TextCache,
    fonts: &Fonts,
    rows: &[FocusRow],
    width: u32,
    open_dropdown_row: Option<usize>,
) -> Result<Painter> {
    let height = rows.len() as u32 * (SETTINGS_ROW_H + SETTINGS_ROW_GAP as u32);
    let mut p = Painter::new(width, height.max(1));
    draw_focus_rows(
        &mut p,
        text_cache,
        fonts,
        rows,
        usize::MAX,
        open_dropdown_row,
        Rect::new(0, 0, width, height),
    )?;
    Ok(p)
}

/// Draws one focus row (icon + label + dropdown pill / slider / modern switch
/// / nothing, per `RowKind`) into `row_rect`, focused or not — shared by
/// `draw_focus_rows` (the static, always-unfocused shell) and
/// `render_focus_row_tile` (the single focused row, recomposited on its own
/// when focus moves or its `Toggle` control animates). `row_rect` is always
/// drawn at its literal, un-zoomed size (see [`draw_selectable`]'s docs on why
/// the zoom lives elsewhere). `dropdown_open` is independent of `focused`: a
/// `Dropdown` row's pill only gets its bright outline while *its own* dropdown
/// is actually expanded, not merely while the row has keyboard focus.
#[allow(clippy::too_many_arguments)]
pub fn draw_focus_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    row: &FocusRow,
    focused: bool,
    dropdown_open: bool,
    switch_frac: f32,
    row_rect: Rect,
) -> Result<()> {
    draw_selectable_fixed(painter, row_rect, focused);

    let icon_pad = 24;
    let icon_rect = Rect::new(
        row_rect.x() + icon_pad,
        row_rect.y() + (row_rect.height() as i32 - SETTINGS_ICON_SIZE as i32) / 2,
        SETTINGS_ICON_SIZE,
        SETTINGS_ICON_SIZE,
    );
    // A destructive row keeps its warning colour whether focused or not — the point is
    // that it reads as dangerous *before* it's the thing about to be confirmed.
    let fg = if row.danger {
        ERROR_RED
    } else if focused {
        WHITE
    } else {
        MUTED
    };
    draw_icon(painter, text_cache, fonts.icon, icon_rect, row.icon, fg)?;
    let label_x = icon_rect.x() + SETTINGS_ICON_SIZE as i32 + 20;
    draw_text(
        painter,
        text_cache,
        fonts.label,
        &row.label,
        label_x,
        row_rect.y() + (row_rect.height() as i32 - fonts.label.height()) / 2,
        fg,
    )?;

    let control_pad = 28;
    match row.kind {
        RowKind::Dropdown => {
            let pill_w = 264u32.min(row_rect.width() / 2);
            let pill = Rect::new(
                row_rect.x() + row_rect.width() as i32 - control_pad - pill_w as i32,
                row_rect.y() + (row_rect.height() as i32 - 52) / 2,
                pill_w,
                52,
            );
            draw_dropdown_pill(painter, text_cache, fonts, pill, &row.value, dropdown_open)?;
        }
        RowKind::Slider => {
            let value_w = fonts.value.size_of(&row.value).map_or(0, |(w, _)| w);
            let slot_right = row_rect.x() + row_rect.width() as i32 - control_pad;
            draw_text(
                painter,
                text_cache,
                fonts.value,
                &row.value,
                slot_right - value_w as i32,
                row_rect.y() + (row_rect.height() as i32 - fonts.value.height()) / 2,
                if focused { WHITE } else { MUTED },
            )?;
            let track_w = 220u32.min(row_rect.width() / 3);
            let track = Rect::new(
                slot_right - SLIDER_VALUE_SLOT_W - track_w as i32,
                row_rect.y() + (row_rect.height() as i32 - 10) / 2,
                track_w,
                10,
            );
            draw_slider_with_thumb(painter, track, row.fraction, focused);
        }
        RowKind::Toggle => {
            let switch = Rect::new(
                row_rect.x() + row_rect.width() as i32 - control_pad - 64,
                row_rect.y() + (row_rect.height() as i32 - 34) / 2,
                64,
                34,
            );
            draw_switch(painter, switch, switch_frac);
        }
        // No control at all — Confirm on the row is the action. `value`, when set,
        // is a muted right-aligned hint (an address, a state), never interactive.
        RowKind::Action => {
            if !row.value.is_empty() {
                let value_w = fonts.value.size_of(&row.value).map_or(0, |(w, _)| w);
                draw_text(
                    painter,
                    text_cache,
                    fonts.value,
                    &row.value,
                    row_rect.x() + row_rect.width() as i32 - control_pad - value_w as i32,
                    row_rect.y() + (row_rect.height() as i32 - fonts.value.height()) / 2,
                    MUTED,
                )?;
            }
        }
    }
    Ok(())
}

/// A rounded pill button showing the current dropdown value + a small chevron
/// (`ICON_CHEVRON_DOWN`, replacing a hand-drawn triangle — see the icons section).
/// `open` gets the bright outline only while this pill's own dropdown overlay
/// is actually expanded — not while the row merely has keyboard focus.
pub fn draw_dropdown_pill(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    rect: Rect,
    label: &str,
    open: bool,
) -> Result<()> {
    let radius = rect.height() as i32 / 2;
    painter.fill_rounded_rect(rect, radius, Color::RGBA(0xff, 0xff, 0xff, 0x12));
    painter.stroke_rounded_rect(
        rect,
        radius,
        if open {
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
    draw_icon(painter, text_cache, fonts.icon, chevron_rect, ICON_CHEVRON_DOWN, WHITE)?;
    let text_w = fonts.value.size_of(label).map_or(0, |(w, _)| w);
    let text_x = rect.x() + ((rect.width() as i32 - chevron_size as i32 - chevron_pad) - text_w as i32) / 2;
    draw_text(
        painter,
        text_cache,
        fonts.value,
        label,
        text_x.max(rect.x()),
        rect.y() + (rect.height() as i32 - fonts.value.height()) / 2,
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

/// Linear interpolation between two colors (including alpha), `frac` clamped
/// to `0.0..=1.0` — used to cross-fade the switch track color as it slides.
pub fn lerp_color(from: Color, to: Color, frac: f32) -> Color {
    let f = frac.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * f) as u8;
    Color::RGBA(
        lerp(from.r, to.r),
        lerp(from.g, to.g),
        lerp(from.b, to.b),
        lerp(from.a, to.a),
    )
}

pub const SWITCH_OFF_TRACK: Color = Color::RGBA(0xff, 0xff, 0xff, 0x22);

/// A modern sliding pill switch (iOS/Android-style) — accent-filled track with
/// the knob at the right when on, muted track with the knob at the left when
/// off. `frac` (0.0 = off, 1.0 = on) lerps the knob position and track color
/// between those two states, so a toggle flip can animate as a slide instead
/// of an instant snap — pass a static `0.0`/`1.0` for an unanimated switch.
pub fn draw_switch(painter: &mut Painter, rect: Rect, frac: f32) {
    let frac = frac.clamp(0.0, 1.0);
    let radius = rect.height() as i32 / 2;
    painter.fill_rounded_rect(rect, radius, lerp_color(SWITCH_OFF_TRACK, ACCENT, frac));
    let knob_r = radius as f32 - 4.0;
    let cy = rect.y() as f32 + rect.height() as f32 / 2.0;
    let left = rect.x() as f32 + radius as f32;
    let right = rect.x() as f32 + rect.width() as f32 - radius as f32;
    let cx = left + (right - left) * frac;
    painter.fill_circle(cx + 1.0, cy + 2.0, knob_r, Color::RGBA(0x00, 0x00, 0x00, 0x40));
    painter.fill_circle(cx, cy, knob_r, WHITE);
}

/// Row height of one dropdown option — also `render_dropdown_option_tile`'s tile size.
pub const DROPDOWN_OPTION_H: u32 = 56;

/// A track+thumb along a scrollable list's right edge. `total`/`visible`/`scroll` are row
/// counts (`visible` <= `total`, `scroll` <= `total - visible`). Rendered into its own
/// tile so the fade-in/out is a per-frame alpha composite, not a re-rasterize.
const SCROLLBAR_TRACK_W: u32 = 6;

pub fn render_list_scrollbar_tile(tile_w: u32, tile_h: u32, total: usize, visible: usize, scroll: usize) -> Painter {
    let mut painter = Painter::new(tile_w, tile_h.max(1));
    if total <= visible {
        return painter;
    }
    let track_w = SCROLLBAR_TRACK_W.min(tile_w);
    let track = Rect::new(tile_w as i32 - track_w as i32, 0, track_w, tile_h);
    painter.fill_rounded_rect(track, track_w as i32 / 2, Color::RGBA(0xff, 0xff, 0xff, 0x14));

    let thumb_h = ((visible as f32 / total as f32) * track.height() as f32).round() as u32;
    let thumb_h = thumb_h.clamp(24, track.height());
    let max_thumb_y = track.height().saturating_sub(thumb_h) as f32;
    let max_scroll = (total - visible).max(1) as f32;
    let thumb_y = track.y() + ((scroll as f32 / max_scroll) * max_thumb_y).round() as i32;
    let thumb = Rect::new(track.x(), thumb_y, track_w, thumb_h);
    painter.fill_rounded_rect(thumb, track_w as i32 / 2, Color::RGBA(0xff, 0xff, 0xff, 0x50));
    painter
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
    let bg_rect = Rect::new(
        rect.x(),
        rect.y(),
        rect.width(),
        options.len() as u32 * DROPDOWN_OPTION_H,
    );
    draw_popup_panel(painter, bg_rect, Color::RGBA(0xff, 0xff, 0xff, 0x20));
    for (i, opt) in options.iter().enumerate() {
        let row_rect = dropdown_option_rect(rect, i);
        draw_dropdown_option(painter, text_cache, font_value, opt, i == focused_index, row_rect)?;
    }
    Ok(())
}

/// Option `index`'s rect within a dropdown overlay anchored at `rect` — the one
/// place this layout formula lives, shared by `draw_dropdown_overlay` and
/// `app.rs`'s `draw_list` (which needs it to position the composited
/// focused-option tile).
pub fn dropdown_option_rect(rect: Rect, index: usize) -> Rect {
    Rect::new(
        rect.x(),
        rect.y() + index as i32 * DROPDOWN_OPTION_H as i32,
        rect.width(),
        DROPDOWN_OPTION_H,
    )
}

/// One dropdown option, focused, as its own tile (no padding needed — unlike
/// the settings-row focus tile, this highlight has no shadow/zoom overflowing
/// its row rect) — composited by the GPU over the overlay's unfocused option
/// list. Moving the dropdown's own focus recomposites just this small tile
/// instead of re-rasterizing the whole modal.
pub fn render_dropdown_option_tile(
    text_cache: &mut TextCache,
    font_value: &Font,
    option: &str,
    width: u32,
) -> Result<Painter> {
    let mut p = Painter::new(width, DROPDOWN_OPTION_H);
    let rect = Rect::new(0, 0, width, DROPDOWN_OPTION_H);
    draw_dropdown_option(&mut p, text_cache, font_value, option, true, rect)?;
    Ok(p)
}

/// Draws one dropdown option (highlight when focused + its label) into
/// `row_rect` — shared by `draw_dropdown_overlay` (the static, always-unfocused
/// list) and `render_dropdown_option_tile` (the single focused option,
/// recomposited on its own when the dropdown's focus moves).
pub fn draw_dropdown_option(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    font_value: &Font,
    option: &str,
    focused: bool,
    row_rect: Rect,
) -> Result<()> {
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
        option,
        row_rect.x() + 20,
        row_rect.y() + (row_rect.height() as i32 - font_value.height()) / 2,
        if focused { WHITE } else { MUTED },
    )?;
    Ok(())
}

/// The floating-panel chrome shared by every popup menu drawn over Home/the
/// modals — a shadowed, near-black rounded panel with a colored border.
/// Extracted from [`draw_dropdown_overlay`], which used to carry its own copy
/// of this same triple (shadow, fill, stroke).
pub fn draw_popup_panel(painter: &mut Painter, rect: Rect, border_color: Color) {
    draw_card_shadow(painter, rect, CARD_RADIUS);
    painter.fill_rounded_rect(rect, CARD_RADIUS, Color::RGBA(0x17, 0x11, 0x28, 0xf6));
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

/// Gap between the two buttons in a [`draw_confirm_buttons`] row.
const CONFIRM_BUTTON_GAP: i32 = 20;

/// A confirm button's interior metrics — icon size, icon-to-label gap, and the
/// padding at each end — all derived from the label font's line height, which
/// [`load_font`] already scales by panel height. One place, so the drawing code
/// and [`confirm_row_min_width`] can never disagree about how much room a label
/// actually gets.
fn confirm_button_metrics(font: &Font) -> (u32, i32, i32) {
    let line_h = font.height().max(1);
    ((line_h * 2 / 3).max(1) as u32, (line_h / 3).max(1), (line_h / 2).max(1))
}

/// The narrowest `content` rect that shows both `buttons` labels in full.
///
/// A confirmation dialog is as wide as its buttons need to be: the labels are
/// real words in whatever length they happen to be, and the card's width is a
/// fraction of the screen, so the two are otherwise unrelated — which is how
/// "Stop streaming" came to be ellipsized inside a 34%-wide card at 1080p while
/// fitting at 4K. Callers take the max of this and their own preferred width.
pub fn confirm_row_min_width(font: &Font, buttons: &[ConfirmButton; 2]) -> u32 {
    let (icon_size, icon_gap, side_pad) = confirm_button_metrics(font);
    let widest = buttons
        .iter()
        .map(|b| {
            let label_w = font.size_of(b.label).map_or(0, |(w, _)| w);
            let leading = if b.icon.is_some() {
                icon_size + icon_gap as u32
            } else {
                0
            };
            label_w + leading + 2 * side_pad as u32
        })
        .max()
        .unwrap_or(0);
    widest * 2 + CONFIRM_BUTTON_GAP as u32
}

/// Button `index`'s rect within a [`draw_confirm_buttons`] row anchored at
/// `content` — the one place this side-by-side layout formula lives, shared
/// by `draw_confirm_buttons` and `app.rs`'s `draw_list` (which needs it to
/// position the composited focused-button tile).
pub fn confirm_button_rect(content: Rect, index: usize) -> Rect {
    let gap = CONFIRM_BUTTON_GAP;
    let btn_w = content.width().saturating_sub(gap as u32) / 2;
    Rect::new(
        content.x() + index as i32 * (btn_w as i32 + gap),
        content.y(),
        btn_w,
        content.height(),
    )
}

/// A row of side-by-side buttons for a Yes/No-style confirmation (currently
/// just the "Forget this host?" dialog's Forget/Cancel pair, but not written
/// specifically for that) — an optional leading icon and a label colored by
/// that button's own identity when focused, or [`MUTED`] otherwise.
/// `focused_index` picks which of `buttons` has focus; every button renders
/// at its normal, un-zoomed size (see [`draw_selectable_fixed`]'s docs on why
/// the zoom lives elsewhere, in `app.rs`'s `draw_list`).
pub fn draw_confirm_buttons(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    content: Rect,
    buttons: &[ConfirmButton; 2],
    focused_index: usize,
) -> Result<()> {
    for (i, button) in buttons.iter().enumerate() {
        let rect = confirm_button_rect(content, i);
        draw_confirm_button(painter, text_cache, fonts, button, i == focused_index, rect)?;
    }
    Ok(())
}

/// One focused confirm button, as its own padded transparent tile —
/// composited by the GPU over the shell (which draws every button unfocused
/// via `draw_confirm_buttons`, see its docs). Mirrors `render_focus_row_tile`'s
/// settings-row equivalent.
pub fn render_confirm_button_tile(
    text_cache: &mut TextCache,
    fonts: &Fonts,
    button: &ConfirmButton<'_>,
    w: u32,
    h: u32,
) -> Result<Painter> {
    let pad = ROW_TILE_PAD;
    let rect = Rect::new(pad, pad, w, h);
    let mut p = Painter::new(w + 2 * pad as u32, h + 2 * pad as u32);
    draw_confirm_button(&mut p, text_cache, fonts, button, true, rect)?;
    Ok(p)
}

/// Draws one confirm button into `rect`, focused or not — shared by
/// `draw_confirm_buttons` (the static, always-unfocused shell) and
/// `render_confirm_button_tile` (the single focused button, recomposited on
/// its own when focus moves).
pub fn draw_confirm_button(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    button: &ConfirmButton<'_>,
    focused: bool,
    rect: Rect,
) -> Result<()> {
    draw_selectable_fixed(painter, rect, focused);
    let color = if focused { button.color } else { MUTED };

    // Every inset here is derived from the label font's own line height, which
    // `load_font` already scales by the panel's height — the button's width scales with
    // the screen too, so a hardcoded icon inset does not stay in proportion to either.
    // It used to be a fixed `20 + 26 + 12`, which left "Stop streaming" more label than
    // button below 4K (~117px of room for ~154px of text at 720p) and ran it past the
    // right edge, because nothing clamped the label either.
    let line_h = fonts.label.height().max(1);
    let (icon_size, icon_gap, side_pad) = confirm_button_metrics(fonts.label);

    // Icon and label are centred as one group, the same way a label without an icon
    // was already centred on its own — and the label is ellipsized to whatever the icon
    // leaves, so no label can overflow the button regardless of resolution.
    let leading = match button.icon {
        Some(_) => icon_size + icon_gap as u32,
        None => 0,
    };
    let budget = rect.width().saturating_sub(2 * side_pad as u32).saturating_sub(leading);
    let label = ellipsize(fonts.label, button.label, budget);
    let label_w = fonts.label.size_of(&label).map_or(0, |(w, _)| w);
    let start_x = rect.x() + (rect.width() as i32 - (leading + label_w) as i32) / 2;

    if let Some(icon) = button.icon {
        let icon_rect = Rect::new(
            start_x,
            rect.y() + (rect.height() as i32 - icon_size as i32) / 2,
            icon_size,
            icon_size,
        );
        draw_icon(painter, text_cache, fonts.icon, icon_rect, icon, color)?;
    }
    draw_text(
        painter,
        text_cache,
        fonts.label,
        &label,
        start_x + leading as i32,
        rect.y() + (rect.height() as i32 - line_h) / 2,
        color,
    )?;
    Ok(())
}
