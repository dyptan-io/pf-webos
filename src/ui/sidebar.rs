//! The host-list sidebar: brand lockup, host rows, utility rows.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use crate::discovery::DiscoveredHost;
use crate::store::KnownHost;
use anyhow::Result;
use sdl2::pixels::Color;
use sdl2::rect::Rect;

// Sized for a 10-foot TV viewing distance, not a desktop/phone screen.
pub const SIDEBAR_W: u32 = 460;
pub const SIDEBAR_PAD: i32 = 24;
pub const SIDEBAR_TOP_Y: i32 = 216;
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

/// Size of the "more actions" (⋯) hit target at the right end of a host row. Square,
/// and generous — this is a 10-foot UI driven by a wobbly pointer, so the touch target
/// is deliberately much larger than the glyph drawn inside it.
pub const SIDEBAR_MENU_BTN: u32 = 52;
/// The glyph itself, inset within that target.
const SIDEBAR_MENU_GLYPH: u32 = 26;
/// Diameter of the presence dot badged onto a host row's icon.
const PRESENCE_DOT: f32 = 9.0;

/// The ⋯ actions button's rect within a host row. Right-aligned inside the row, so it
/// reads as belonging to that host rather than to the panel.
pub fn sidebar_menu_button_rect(row_rect: Rect) -> Rect {
    let inset = 10i32;
    Rect::new(
        row_rect.x() + row_rect.width() as i32 - SIDEBAR_MENU_BTN as i32 - inset,
        row_rect.y() + (row_rect.height() as i32 - SIDEBAR_MENU_BTN as i32) / 2,
        SIDEBAR_MENU_BTN,
        SIDEBAR_MENU_BTN,
    )
}

/// Whether `(x, y)` is on host row `index`'s ⋯ button. Checked *before*
/// [`hit_test_sidebar_row`] by the click handler, since the button sits inside the row
/// it belongs to and would otherwise just read as a click on the row.
pub fn hit_test_sidebar_menu_button(x: i32, y: i32, host_count: usize) -> Option<usize> {
    (0..host_count).find(|&i| sidebar_menu_button_rect(sidebar_row_rect(i)).contains_point((x, y)))
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

/// Draw a selectable row with optional selection highlighting. When focused, shows
/// the full card with shadow and zoom. When selected (but not focused), shows a
/// subtle background. When neither, shows no background.
fn draw_selectable_with_selection(painter: &mut Painter, rect: Rect, focused: bool, selected: bool) -> Rect {
    let r = draw_selectable(painter, rect, focused);
    if !focused && selected {
        let selected_bg = Color::RGBA(0x2b, 0x21, 0x48, 0x40);
        painter.fill_rounded_rect(r, CARD_RADIUS, selected_bg);
    }
    r
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
/// itself has focus (see `app.rs`'s `HomeFocus`). `selected_index` highlights
/// the currently-selected host row to indicate it's the active/connected host.
#[allow(clippy::too_many_arguments)]
pub fn draw_sidebar(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    entries: &[HostEntry],
    focused_index: Option<usize>,
    selected_index: Option<usize>,
    // `online` is index-aligned with `entries`; `None` = not probed yet (see `app::reach`).
    online: &[Option<bool>],
    screen_h: u32,
) -> Result<()> {
    painter.fill_rect(Rect::new(0, 0, SIDEBAR_W, screen_h), SIDEBAR_BG);
    // The real brand lockup (mark + FUNK wordmark), from the actual logo
    // artwork — see `logo_pixmap`. The bundled asset is exported at exactly
    // its on-screen display size (rendered fresh from
    // `punktfunk-logo-dark.svg` at that size, not a scaled copy of a smaller
    // export — see its NOTICE.md), so this draws it 1:1, no runtime scaling
    // in either direction. Centered horizontally.
    if let Some(logo) = logo_pixmap() {
        let logo_x = (SIDEBAR_W as i32 - logo.width() as i32) / 2;
        painter.draw_pixmap(logo_x, 32, logo);
    }

    let add_row = entries.len();
    let settings_row = entries.len() + 1;
    for (i, entry) in entries.iter().enumerate() {
        draw_host_row(
            painter,
            text_cache,
            fonts,
            sidebar_row_rect(i),
            entry.name(),
            entry.is_paired(),
            focused_index == Some(i),
            selected_index == Some(i),
            false,
            online.get(i).copied().flatten(),
        )?;
    }
    draw_utility_row(
        painter,
        text_cache,
        fonts,
        sidebar_row_rect(add_row),
        "+ Add host",
        focused_index == Some(add_row),
    )?;

    let settings_rect = settings_row_rect(screen_h);
    // The build version deliberately does NOT live here any more: every other
    // punktfunk client shows it on its About/licenses screen, not in the nav
    // chrome, and this sidebar is navigation. See `ui::about::VERSION`, surfaced
    // by `Screen::About` (reached from Settings).
    painter.fill_rect(
        Rect::new(settings_rect.x(), settings_rect.y() - 14, settings_rect.width(), 1),
        Color::RGBA(0xff, 0xff, 0xff, 0x1a),
    );
    draw_utility_row(
        painter,
        text_cache,
        fonts,
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
/// background at all. `selected` adds a subtle background when not focused.
/// Host rows and utility rows used to each carry their own near-identical copy
/// of this (differing only by accident of drift, in icon size/padding, not by
/// design).
#[allow(clippy::too_many_arguments)]
pub fn draw_sidebar_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    rect: Rect,
    glyph: &str,
    label: &str,
    focused: bool,
    selected: bool,
    reserve_right: u32,
) -> Result<()> {
    let drawn = draw_selectable_with_selection(painter, rect, focused, selected);
    let icon_size = 30u32;
    let icon_pad = 20;
    let icon_rect = Rect::new(
        drawn.x() + icon_pad,
        drawn.y() + (drawn.height() as i32 - icon_size as i32) / 2,
        icon_size,
        icon_size,
    );
    let color = if focused { WHITE } else { MUTED };
    draw_icon(painter, text_cache, fonts.icon, icon_rect, glyph, color)?;
    // Ellipsized to the row's real text width (icon + paddings subtracted) — a
    // long mDNS hostname used to run past the row/panel edge.
    let text_x = icon_pad + icon_size as i32 + 16;
    // `reserve_right` keeps a long hostname from running underneath the ⋯ button.
    let max_w = drawn.width().saturating_sub(text_x as u32 + 20 + reserve_right);
    let label = ellipsize(fonts.label, label, max_w);
    draw_text(
        painter,
        text_cache,
        fonts.label,
        &label,
        drawn.x() + text_x,
        drawn.y() + (drawn.height() as i32 - fonts.label.height()) / 2,
        color,
    )?;
    Ok(())
}

/// A host row, including its always-visible ⋯ actions button.
///
/// The button is drawn on *every* host row, not just the focused one: it exists to
/// advertise that per-host actions are there at all. (It replaced a hold-OK gesture,
/// which worked but nothing on screen ever said so.) `menu_focused` highlights the
/// button itself — the row can be focused with the button not, and vice versa.
/// `selected` adds a subtle background to indicate the currently-active host.
#[allow(clippy::too_many_arguments)]
pub fn draw_host_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    rect: Rect,
    name: &str,
    paired: bool,
    focused: bool,
    selected: bool,
    menu_focused: bool,
    online: Option<bool>,
) -> Result<()> {
    let glyph = if paired { ICON_TV } else { ICON_LOCK };
    draw_sidebar_row(
        painter,
        text_cache,
        fonts,
        rect,
        glyph,
        name,
        focused,
        selected,
        SIDEBAR_MENU_BTN + 10,
    )?;
    // Badged onto the icon's corner rather than given its own column: it needs no layout
    // of its own, and a presence dot on the thing it describes is a well-worn idiom.
    // `None` (never probed yet) draws nothing at all — an unknown state must not look
    // like a confident "offline".
    if let Some(online) = online {
        let drawn = inflate(rect, focused);
        let icon_size = 30i32;
        let icon_pad = 20i32;
        let cx = drawn.x() as f32 + icon_pad as f32 + icon_size as f32 - 1.0;
        let cy = drawn.y() as f32 + (drawn.height() as f32 + icon_size as f32) / 2.0 - 2.0;
        // A ring of panel background first, so the dot reads as separate from the glyph
        // it overlaps rather than merging into it.
        painter.fill_circle(cx, cy, PRESENCE_DOT / 2.0 + 2.0, SIDEBAR_BG);
        let color = if online { ONLINE_GREEN } else { MUTED };
        painter.fill_circle(cx, cy, PRESENCE_DOT / 2.0, color);
    }
    draw_sidebar_menu_button(painter, text_cache, fonts, rect, focused, menu_focused)
}

/// The ⋯ button itself: a rounded highlight plate once it has focus, then the glyph.
pub fn draw_sidebar_menu_button(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
    row_rect: Rect,
    row_focused: bool,
    menu_focused: bool,
) -> Result<()> {
    let btn = sidebar_menu_button_rect(row_rect);
    if menu_focused {
        painter.fill_rounded_rect(btn, (SIDEBAR_MENU_BTN / 2) as i32, ACCENT);
    }
    let glyph_rect = Rect::new(
        btn.x() + (btn.width() as i32 - SIDEBAR_MENU_GLYPH as i32) / 2,
        btn.y() + (btn.height() as i32 - SIDEBAR_MENU_GLYPH as i32) / 2,
        SIDEBAR_MENU_GLYPH,
        SIDEBAR_MENU_GLYPH,
    );
    let color = if menu_focused || row_focused { WHITE } else { MUTED };
    draw_icon(painter, text_cache, fonts.icon, glyph_rect, ICON_MORE, color)
}

pub fn draw_utility_row(
    painter: &mut Painter,
    text_cache: &mut TextCache,
    fonts: &Fonts,
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
    draw_sidebar_row(painter, text_cache, fonts, rect, glyph, label, focused, false, 0)
}
