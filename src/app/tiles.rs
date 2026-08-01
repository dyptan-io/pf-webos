//! The GPU tile cache: the rasterized-once `Painter` layers (plus the derived card size)
//! that `App::prepare_tiles`/`draw_list` rebuild and compose per frame.
//!
//! Grouped out of `App` so screen state and the render cache are separable. Deliberately
//! holds no reference to `App` — it names only presentation/`core` types — so a later stage
//! can lift it into `ui` behind a `RenderInput` without dragging `App` along.

use super::*;

/// The 17 rasterized-once tile sources for the GPU compositor (`compositor.rs`), keyed as
/// each render path needs. `prepare_tiles` rebuilds whichever are stale and reports them for
/// upload; `draw_list` composes each frame from their textures. Focus movement, scrolling,
/// and animations never re-rasterize anything.
pub(crate) struct TileCache {
    /// Focus-free sidebar strip (`SIDEBAR_W` × screen height): panel, brand mark +
    /// wordmark, every row unfocused. Stale when row content changes (`sidebar_dirty`),
    /// never on focus movement.
    pub(crate) sidebar_layer: Option<Painter>,
    /// Per-card tiles (shadow baked in, transparent padding), keyed by pin id
    /// (a `GameEntry::id`, or `store::DESKTOP_PIN_ID`) rather than grid index —
    /// a pin/unpin reorder only shuffles which index a game sits at, so keying
    /// by identity means the reorder never has to rebuild anything. Absent = not
    /// yet rasterized (or evicted).
    pub(crate) card_tiles: std::collections::HashMap<String, CardTile>,
    /// The shared focus-ring glow tile (one per card size).
    pub(crate) ring_tile: Option<Painter>,
    /// The shared card-outline tile (one per card size) — composited on top of the
    /// focused card's art, unlike `ring_tile` which sits behind it.
    pub(crate) outline_tile: Option<Painter>,
    /// The shared pinned badge tile — built once (it doesn't depend on card size),
    /// composited over the focused card when that card is pinned.
    pub(crate) pin_badge_tile: Option<Painter>,
    /// The focused sidebar row's tile, keyed by row index.
    pub(crate) focused_row_tile: Option<((usize, bool), Painter)>,
    /// The active modal rasterized full-screen (transparent surroundings). Always the
    /// *shell* — every selectable widget drawn unfocused — with the focused one composited
    /// on top from `modal_focus_tile` (see `ModalFocusKey`'s docs).
    pub(crate) modal_tile: Option<Painter>,
    /// The single focused, zoom-animated widget of whichever modal is open —
    /// see `ModalFocusKey`'s docs on why one tile/key suffices for all of them.
    pub(crate) modal_focus_tile: Option<(ModalFocusKey, Painter)>,
    /// Dropdown overlay panel, keyed by (Screen, row) to disambiguate row 0 across
    /// Settings vs Diagnostics. Composited after `ScrollContent`.
    pub(crate) dropdown_overlay_tile: Option<((Screen, usize), Painter)>,
    /// Dropdown's focused option tile, keyed by (Screen, row, focused index).
    /// Composited over `DropdownOverlay`; focus movement rebuilds only this.
    pub(crate) dropdown_focus_tile: Option<((Screen, usize, usize), Painter)>,
    /// Whichever scrollable modal's indicator is baked, keyed by `(total units,
    /// visible units, scroll offset)`. One slot for all of them.
    pub(crate) scroll_indicator_tile: Option<((usize, usize, usize), Painter)>,
    /// Whichever scrollable modal's content is baked, at full (unscrolled) height —
    /// keyed by `(Screen, ScrollContentKey)`. Scrolling within the baked window never
    /// invalidates this.
    pub(crate) scroll_content_tile: Option<((Screen, ScrollContentKey), Painter)>,
    /// The bottom scroll fade. Unkeyed and built at most once per run: a fixed-size alpha
    /// ramp the GPU stretches to each list's width.
    pub(crate) scroll_fade_tile: Option<Painter>,
    /// The mirrored fade for the top edge.
    pub(crate) scroll_fade_top_tile: Option<Painter>,
    /// Home's status line block, keyed by its text.
    pub(crate) status_tile: Option<(String, Painter)>,
    /// The static "No host selected" hint line.
    pub(crate) nohost_tile: Option<Painter>,
}

impl TileCache {
    pub(crate) fn new() -> Self {
        Self {
            sidebar_layer: None,
            card_tiles: std::collections::HashMap::new(),
            ring_tile: None,
            outline_tile: None,
            pin_badge_tile: None,
            focused_row_tile: None,
            modal_tile: None,
            modal_focus_tile: None,
            dropdown_overlay_tile: None,
            dropdown_focus_tile: None,
            scroll_indicator_tile: None,
            scroll_content_tile: None,
            scroll_fade_tile: None,
            scroll_fade_top_tile: None,
            status_tile: None,
            nohost_tile: None,
        }
    }
}
