//! Pre-stream UI: sidebar (known hosts/Settings) + detail grid + modal cards (Pairing/Add host).
//! Renders via Painter (`tiny_skia` software rasterizer) to SDL2 texture (one per frame).
//! Text in Geist font; icons from bundled subsetted font.

mod about;
mod addhost;
mod animation;
mod cards;
mod fade;
mod grid;
mod listmodal;
mod modal;
mod notification;
mod painter;
mod pairing;
pub mod render;
mod render_input;
mod rows;
mod scroll;
mod settings;
mod sidebar;
mod text;
mod text_raster;
mod theme;
mod tiles;

pub use crate::core::event::MenuEvent;
pub use about::*;
pub use addhost::*;
pub use animation::*;
pub use cards::*;
pub use fade::*;
pub use grid::*;
pub use listmodal::*;
pub use modal::*;
pub use notification::*;
pub use painter::*;
pub use pairing::*;
pub use render_input::*;
pub use rows::*;
pub use scroll::*;
pub use settings::*;
pub use sidebar::*;
pub use text::*;
pub use text_raster::{FontId, TextRaster};
pub use theme::*;
pub use tiles::*;
