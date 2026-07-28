//! Pre-stream UI: sidebar (known hosts/Settings) + detail grid + modal cards (Pairing/Add host).
//! Renders via Painter (`tiny_skia` software rasterizer) to SDL2 texture (one per frame).
//! Text in Geist font; icons from bundled subsetted font.

mod about;
mod addhost;
mod animation;
mod cards;
mod fade;
mod grid;
mod input;
mod listmodal;
mod modal;
mod painter;
mod pairing;
mod rows;
mod scroll;
mod settings;
mod sidebar;
mod text;
mod theme;
mod tiles;

pub use about::*;
pub use addhost::*;
pub use animation::*;
pub use cards::*;
pub use fade::*;
pub use grid::*;
pub use input::*;
pub use listmodal::*;
pub use modal::*;
pub use painter::*;
pub use pairing::*;
pub use rows::*;
pub use scroll::*;
pub use settings::*;
pub use sidebar::*;
pub use text::*;
pub use theme::*;
pub use tiles::*;
