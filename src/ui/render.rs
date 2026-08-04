//! UI-native replacements for the `sdl2::rect`/`sdl2::pixels` types that used to
//! leak into `ui::DrawCmd`. `platform::webos::compositor` converts to/from SDL at
//! the boundary; `ui`/`app` only ever see these.

/// Integer rectangle. API mirrors the subset of `sdl2::rect::Rect` this crate used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn x(&self) -> i32 {
        self.x
    }

    pub fn y(&self) -> i32 {
        self.y
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }

    pub fn right(&self) -> i32 {
        self.x + self.w as i32
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h as i32
    }

    pub fn offset(self, dx: i32, dy: i32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn contains_point(&self, p: (i32, i32)) -> bool {
        let (px, py) = p;
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Overlap of `self` and `other`, or `None` if they don't intersect (matches
    /// `sdl2::rect::Rect::intersection`).
    pub fn intersection(&self, other: Self) -> Option<Self> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = self.right().min(other.right());
        let y2 = self.bottom().min(other.bottom());
        if x2 <= x1 || y2 <= y1 {
            return None;
        }
        Some(Self::new(x1, y1, (x2 - x1) as u32, (y2 - y1) as u32))
    }
}

/// Float rectangle, for the one case where whole-pixel placement is too coarse: a pan
/// slow enough that an integer destination would advance in visible jumps rather than
/// drift (see `DrawCmd::TexF`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Straight-alpha RGBA8. Mirrors `sdl2::pixels::Color`'s public field layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[allow(non_snake_case)]
impl Color {
    pub const fn RGBA(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    pub const fn RGB(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

/// Texture cache key. Same variants as the former `compositor::Tile`, now living in
/// `ui` and keyed by the plain `core::screen::Screen` enum rather than a platform type.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TileId {
    Sidebar,
    FocusRow,
    Card(String),
    Ring,
    CardOutline,
    PinBadge,
    Modal,
    ModalFocusElement,
    DropdownOverlay,
    DropdownFocusOption,
    Status,
    NoHost,
    ScrollIndicator(crate::core::screen::Screen),
    ScrollContent(crate::core::screen::Screen),
    ScrollFade,
    ScrollFadeTop,
    SpinnerFrame(usize),
    /// Wide hero art for the connecting screen, keyed by game id.
    Hero(String),
    StatsOverlay,
    Notification,
    LogOverlay,
    DisconnectDialog,
    DisconnectFocusButton,
}

/// One step of a frame's composition, in paint order.
pub enum DrawCmd {
    Tex {
        tile: TileId,
        dst: Rect,
        alpha: u8,
    },
    TexCropped {
        tile: TileId,
        src: Rect,
        dst: Rect,
        alpha: u8,
    },
    /// Whole texture to a subpixel destination, clipped by the render target. Bilinear
    /// filtering turns the fractional offset into real subpixel motion, which is what
    /// makes a very slow pan look continuous instead of stepped.
    TexF {
        tile: TileId,
        dst: RectF,
        alpha: u8,
    },
    Fill {
        rect: Rect,
        color: Color,
    },
}

pub type DrawList = Vec<DrawCmd>;
