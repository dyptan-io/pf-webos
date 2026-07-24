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
//! Text renders in punktfunk's brand font, Geist (bundled — see `load_font`);
//! icons are glyphs from a small bundled, subsetted icon font (see the icons
//! section below and `assets/icons/NOTICE.md`).

mod animation;
mod theme;
mod input;
mod painter;
mod text;
mod cards;
mod tiles;
mod sidebar;
mod grid;
mod modal;
mod pairing;
mod rows;
mod settings;
mod addhost;
mod listmodal;
mod about;

// Glob re-exports: every item keeps its original `crate::ui::X` path, so splitting
// this module needed no changes at any call site in `app.rs`/`main.rs`.
pub use animation::*;
pub use theme::*;
pub use input::*;
pub use painter::*;
pub use text::*;
pub use cards::*;
pub use tiles::*;
pub use sidebar::*;
pub use grid::*;
pub use modal::*;
pub use pairing::*;
pub use rows::*;
pub use settings::*;
pub use addhost::*;
pub use listmodal::*;
pub use about::*;
