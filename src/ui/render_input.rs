//! `RenderInput` — the read-only slice of `App` state the render path consumes.
//!
//! The renderer (`prepare_tiles`/`draw_list`, today still on `App`) is being lifted onto the
//! tile cache so it takes app state as *input* rather than reading `App` directly (see
//! `docs/REMAINING_IMPROVEMENTS.md` → A2). `App::render_input` assembles this once per frame;
//! the render methods read `input.<field>` instead of `self.<field>`. Built in `app` (the
//! one-way assembly), consumed here in `ui`, so `ui` stays a dependency leaf.
//!
//! Grown one family at a time: only the fields already migrated off `self` live here.

use crate::core::screen::HomeFocus;
use crate::ui::HostEntry;

pub struct RenderInput<'a> {
    pub home_focus: HomeFocus,
    pub entries: &'a [HostEntry],
    /// A host is selected (grid has content rather than the "no host" hint).
    pub host_selected: bool,
    /// `home_status` is set (the bottom status block is drawn).
    pub has_status: bool,
    /// The grid's cards are built and revealed (past the load spinner).
    pub grid_reveal_ready: bool,
}
