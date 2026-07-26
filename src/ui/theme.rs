//! The punktfunk brand palette and the bundled icon-font glyph constants.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use sdl2::pixels::Color;

// The punktfunk brand palette, sampled from `packaging/icon_large.png` (the
// canonical mark): brand-dark `#1c1530` surfaces, primary purple `#6c5bf3`,
// lavender `#a79ff8`, pale-lavender overlap highlight `#d2c9fb`.

/// App background — a step darker than the brand dark, so panels/cards read as
/// elevated surfaces on top of it.
pub const BG: Color = Color::RGB(0x14, 0x10, 0x1f);
/// Panel/modal surface — the brand dark (`iconColor` in appinfo.json).
pub const SIDEBAR_BG: Color = Color::RGB(0x1c, 0x15, 0x30);
/// Elevated interactive surface (focused rows, PIN/IP entry boxes, dropdown
/// panel) — a purple step lighter than `SIDEBAR_BG` so focus reads on it.
pub const SURFACE: Color = Color::RGB(0x2b, 0x21, 0x48);
/// Brand purple — the icon's primary circle.
pub const ACCENT: Color = Color::RGB(0x6c, 0x5b, 0xf3);
/// Brand lavender — the icon's secondary circle; focus-ring glow, slider fills.
pub const ACCENT_BRIGHT: Color = Color::RGB(0xa7, 0x9f, 0xf8);
pub const WARNING: Color = Color::RGB(0xff, 0xc1, 0x07);
pub const ERROR_RED: Color = Color::RGB(0xff, 0x6b, 0x6b);
/// Host-reachable presence dot. A desaturated mint rather than a signal green — it sits
/// next to brand purple on every row, and a pure green fights it.
pub const ONLINE_GREEN: Color = Color::RGB(0x5c, 0xd6, 0xa0);
pub const WHITE: Color = Color::RGB(0xf5, 0xf5, 0xf5);
/// Secondary text — lavender-grey rather than neutral grey, staying in family.
pub const MUTED: Color = Color::RGB(0x9b, 0x94, 0xb8);
pub const MODAL_SCRIM: Color = Color::RGBA(0x00, 0x00, 0x00, 0x80);

// ------------------------------------------------------------------------ icons --
// Every icon in this UI is a glyph from a bundled, subsetted copy of Google's
// Material Icons font (`assets/icons/MaterialIcons-subset.ttf`, Apache 2.0 — see
// `assets/icons/NOTICE.md` for provenance/license and how to regenerate the subset)
// rather than a vector-drawn shape: a text font's icon coverage is unreliable,
// so real icon glyphs need a font of their own, and a real
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
pub const ICON_EDIT: &str = "\u{E3C9}";
pub const ICON_INFO: &str = "\u{E88E}";
/// The host row's "more actions" affordance — see `sidebar_menu_button_rect`.
pub const ICON_MORE: &str = "\u{E5D3}";
