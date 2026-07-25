//! Game-grid geometry: columns, card rects, hit testing, scroll extent.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use sdl2::rect::Rect;


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

/// `scroll` is the grid's current vertical scroll offset in px (see
/// `App::grid_scroll`) — card rects live in unscrolled layout space, so the
/// pointer's y is translated into that space before testing.
pub fn hit_test_grid_card(
    mouse_x: i32,
    mouse_y: i32,
    columns: usize,
    count: usize,
    grid_x: i32,
    available_w: u32,
    scroll: i32,
) -> Option<usize> {
    if mouse_x < grid_x {
        return None;
    }
    (0..count)
        .find(|&i| grid_card_rect(i, columns, grid_x, available_w).contains_point((mouse_x, mouse_y + scroll)))
}

/// Headroom above/below the card rows inside the cached grid layer (see
/// `App::grid_layer`), so row 0's shadow and the last row's shadow tail have
/// somewhere to land instead of clipping at the layer edge.
pub const GRID_LAYER_PAD: i32 = 24;

/// Total pixel height of the cached grid layer for `count` cards: all rows plus
/// the shadow headroom above and below.
pub fn grid_layer_height(count: usize, columns: usize, available_w: u32) -> u32 {
    let rows = count.div_ceil(columns.max(1));
    let (_, card_h) = grid_card_size(available_w, columns);
    (rows.max(1) as u32 * (card_h + GRID_GAP as u32)) + 2 * GRID_LAYER_PAD as u32
}

