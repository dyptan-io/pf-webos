# Remaining Improvements Plan

Follow-up to the layered refactor (see `CLAUDE.md` → Architecture). The dependency
graph is now acyclic and `core`/`ui`/`services` are platform-independent leaves. What
remains is mostly inside `app/`, plus a few polish items. **Read `docs/NOTES.md` first.**

**Ground rules (unchanged from the last refactor):**
- Behavior must not change. These are structure moves, not feature work. Find a bug → note
  it, don't fix it in the same commit.
- One shippable commit per step; each compiles.
- Verify every step: `task docker:check` (Linux, the shipping target) **and** `cargo check`
  on the macOS host (proves the platform-independent layers still build cross-platform).
  Also `task docker:lint` (clippy is `-D warnings` in CI) and `task fmt`.
- No assistant co-author trailers. WHY-comments only.

Priority order: **A** (highest value, largest), then **B**, **C**, **D** (stretch).

## Status

Done (verified: `docker:check` + `docker:lint` clean, macOS `cargo check` 0 warnings; **not yet
smoke-tested on device**):
- **C2** — crate-root `#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]`.
- **C1** — `Rect::right/bottom/offset` added and adopted (no `center()`: no call site yet).
- **B** — menu-input plumbing moved to `runtime/input.rs` (`pub(super)`, re-exported via `use input::*`).
- **A0** — `App::advance_frame(screen_w) -> bool` now owns the `card_size` + modal-fade
  mutations; `prepare_tiles` gained a `screen_changed` param and only touches tiles. Call order
  in `ui_flow.rs`: `advance_frame` then `prepare_tiles`.
- **A1** — the 17 tile fields live in `app::tiles::TileCache` (App holds one `tiles` field).
  Placed in **`app`, not `ui`** — the key enums `ModalFocusKey`/`ScrollContentKey`/`CardTile`
  it references still live in `app`; relocating them (+ re-pathing `Settings`/`LogLevelOverride`
  to `core`) is A2's job. `TileCache` is already App-agnostic, so A2 can lift it into `ui`.

Remaining: **A2** (move render methods onto the renderer behind `RenderInput`, incl. relocating
the key enums to `ui`), **D** (emulator). Both need on-device screenshot verification.

---

## Current coupling (the thing to fix in A)

`app/mod.rs` (~2970 lines) fuses two responsibilities in one `App` struct:

1. **Screen state** — `screen`, `home_focus`, `entries`, `games`, `settings`, scroll
   offsets, fade/animation timers, dropdown state, etc. (legitimately `App`'s job).
2. **A GPU tile cache** — ~15 keyed `Option<Painter>` fields plus a
   `HashMap<String, CardTile>`, all `tiny_skia`-typed rasterized-once layers:

   ```
   sidebar_layer, ring_tile, outline_tile, pin_badge_tile, focused_row_tile,
   modal_tile, modal_focus_tile, dropdown_overlay_tile, dropdown_focus_tile,
   scroll_indicator_tile, scroll_content_tile, scroll_fade_tile, scroll_fade_top_tile,
   status_tile, nohost_tile, card_tiles: HashMap<String, CardTile>, card_size
   ```

Three methods drive the render loop (all `impl App`, all called only from
`runtime/ui_flow.rs`):

- `prepare_tiles(&mut self, text_cache, fonts, w, h, content_dirty) -> Vec<Tile>` —
  rebuilds whichever `Painter`s are stale; returns the `TileId`s that changed so the
  caller re-uploads only those. **Also mutates app state** (`self.card_size`, and it
  advances the modal open/close fades) — this entanglement is why the cut is staged.
- `draw_list(&self, w, h, fonts) -> ui::render::DrawList` — pure read: composes the
  per-frame `DrawCmd` list from state + the cached `Painter`s' sizes.
- `tile_pixmap(&self, &TileId) -> Option<&Painter>` — lookup used by the upload loop.

Render loop today (`runtime/ui_flow.rs` ~300-369):
```
let updated = app.prepare_tiles(&mut text_cache, fonts, w, h, content_dirty)?;
for tile in updated { compositor.upload(tex_creator, tile, app.tile_pixmap(&tile)?) | drop }
let cmds = app.draw_list(w, h, fonts);
compositor.present(canvas, &cmds)?;
```

**Goal of A:** move the tile cache + these three methods into a dedicated renderer that
takes app state as *input* and owns the `Painter`s, so `App` holds only screen state.

---

## Improvement A — extract the tile cache into a renderer

Staged so each stage compiles and ships. Do NOT try to do it in one cut — `prepare_tiles`
mixes tile rebuilding with app-state mutation, and untangling that is the hard part.

### A0 — Prep: separate app-state mutation out of `prepare_tiles`

Before moving anything, make `prepare_tiles` *only* touch tiles.

1. Find every non-tile mutation inside `prepare_tiles`. Known ones: `self.card_size = …`
   (line ~1533) and the modal fade advancement block near the top (the "every screen
   transition triggers close-fade…" comment). Grep the body for `self.` assignments that
   are not `self.<tile field> = …`.
2. Move those into a new `App::advance_frame(&mut self, w, h)` (or fold into the existing
   per-tick state update in `ui_flow`) called *before* `prepare_tiles`. `card_size` is
   derived from `w`/columns — recompute it there.
3. `prepare_tiles` now reads `self.card_size` etc. as inputs and writes only tile fields.
4. Verify: behavior identical (fades, card pop-in still animate on device).

**Risk:** ordering — fades must advance exactly once per tick, before compose. Keep the
call site order in `ui_flow` explicit and commented.

### A1 — Group the tile fields into one `Tiles` struct (pure move)

1. In `ui/` (this is presentation state) add `ui::tiles` → `pub struct TileCache { … }`
   holding the 17 fields listed above (move them verbatim, keep names and key types like
   `((Screen, usize), Painter)`). Derive nothing; add `TileCache::new()` mirroring the
   current field initialisers in `App::new`.

   > Put it in `ui`, not `app`: these are `tiny_skia` `Painter`s with no `App` knowledge,
   > and keeping them in `ui` is what will eventually let a `ui`-only harness (D) render
   > without `app`. `TileCache` must NOT reference `App`.

2. Add one field `tiles: ui::TileCache` to `App`; delete the 17 individual fields.
3. Mechanically re-path every `self.<tile field>` → `self.tiles.<field>`. The compiler
   lists each site (dozens, all in `app/mod.rs`). No logic change.
4. Verify Linux + macOS + on-device smoke.

### A2 — Move the render methods onto the renderer

Now move behavior. Target shape:

```rust
// ui/render loop lives with the tiles it owns
impl ui::TileCache {
    // was App::prepare_tiles — takes state as input, owns only tiles
    pub fn prepare(&mut self, state: &RenderInput, text_cache: &mut TextCache,
                   fonts: &Fonts) -> Result<Vec<TileId>>;
    // was App::draw_list
    pub fn compose(&self, state: &RenderInput, fonts: &Fonts) -> DrawList;
    // was App::tile_pixmap
    pub fn painter(&self, tile: &TileId) -> Option<&Painter>;
}
```

`RenderInput` is the read-only slice of state the two methods need (screen, home_focus,
games, entries, settings, scroll offsets, fade timers, dropdown state, `card_size`, …).
Two options — pick per how wide the borrow is:

- **A2a (smaller diff):** make `prepare`/`compose` free-standing `fn`s in `ui` taking
  `&mut TileCache` + `&App`. `ui` would then depend on `app` — **not allowed** (breaks the
  leaf). So only acceptable if `RenderInput` is a real struct, not `&App`.
- **A2b (correct):** define `pub struct RenderInput<'a> { … }` in `ui` with borrowed fields,
  build it in `app` (`impl App { fn render_input(&self) -> ui::RenderInput<'_> }`), and hand
  it in. `ui` stays a leaf; `app` does the one-way assembly.

Use **A2b**. Steps:

1. Enumerate exactly what `prepare_tiles`/`draw_list` read from `self` (grep `self\.` in both
   bodies; everything that isn't a tile field is state input). That set becomes
   `RenderInput`'s fields (borrows/copies of primitives).
2. Define `ui::RenderInput<'a>` + `App::render_input(&self) -> RenderInput<'_>`.
3. Move the three method bodies to `impl TileCache`, replacing `self.<state>` with
   `input.<state>` and `self.<tile>` with `self.<tile>` (now the tile is `self`). Move the
   private geometry helpers they call (`scrolled_card_rect`, `grid_layout`,
   `scroll_geometry_for`, `dropdown_geom`, `max_scroll_px`, …) — decide per helper whether it
   is pure geometry (→ `ui`, takes primitives) or reads broad state (→ stays `app`, called to
   build `RenderInput`). Most `*_rect` helpers are pure geometry and belong in `ui`.
4. Update `ui_flow.rs`:
   ```
   let input = app.render_input();
   let updated = tiles.prepare(&input, &mut text_cache, fonts)?;
   for t in updated { compositor.upload(tex_creator, t, tiles.painter(&t)?) | drop }
   let cmds = tiles.compose(&input, fonts);
   ```
   `tiles: ui::TileCache` is now owned by the `ui_flow`/`stream` loop, not `App`.
5. Verify Linux + macOS + full on-device smoke (every screen, scrolling, modal fades, card
   pop-in, focus glow — this method is the whole look of the app).

**Risk:** highest in the project. `draw_list` is ~350 lines of exact geometry; a wrong
offset shifts pixels. Do A2 for **one tile family at a time** if needed (e.g. sidebar first,
then grid cards, then modal/scroll), keeping the old `App` method delegating to the new code
until the last family moves. Screenshot-compare on device before/after.

### A3 — Result

`App` = screen state + `render_input()`. `ui::TileCache` = the renderer. `app/mod.rs` drops
from ~2970 to roughly ~1800; the render engine is reusable without `App` (unblocks D).

---

## Improvement B — split `runtime/mod.rs` input helpers into `runtime/input.rs`

`runtime/mod.rs` (~900 lines) still holds the connect/signal/log-overlay glue **and** all
the menu-input plumbing: `PinHold`, `DisconnectChord`, `ConfirmAction`, `ConfirmDialog`,
`EventAction`, `UiInput`, `exit_gesture_fired`, `text_input_screen`, `edge_trigger_back`,
`pin_hold_gate`, `dispatch_menu_event`, `handle_ui_event`.

1. Move those items into `runtime/input.rs` with `use super::*;`.
2. They are used by both `ui_flow.rs` and `stream.rs` (siblings), so re-export from
   `mod.rs`: `mod input; use input::*;` — sibling modules already do `use super::*`, so they
   pick them up. Mark the items `pub(super)`.
3. Verify Linux + clippy.

Low risk, same mechanics as the ui_flow/stream split already done. Leaves `runtime/mod.rs`
at ~300 lines (run/connect/signals/log-overlay).

---

## Improvement C — UI polish

### C1 — `Rect` convenience helpers
`ui::render::Rect` exposes only `x/y/width/height/contains_point/intersection`. Call sites
recompute `x + w as i32` and `y + h as i32` inline (see `draw_list`, `intersection`, view
geometry). Add and adopt:
```rust
pub fn right(&self) -> i32  { self.x + self.w as i32 }
pub fn bottom(&self) -> i32 { self.y + self.h as i32 }
pub fn center(&self) -> (i32, i32)
pub fn offset(self, dx: i32, dy: i32) -> Self
```
Only add a helper once you have a call site to switch to it (an unused `pub fn` on a leaf is
a dead-code warning on the macOS build — see C2). Convert the inline arithmetic as you go.

### C2 — Silence the macOS-only dead-code warnings
Since `app`/`runtime` are `cfg`-gated out on non-Linux, many `ui` functions they consume look
"never used" on the macOS host build (~430 warnings; Linux build is clean). Options, cheapest
first:
- Leave them — they are host-only and harmless (Linux CI is clean).
- Add `#![cfg_attr(not(target_os = "linux"), allow(dead_code))]` at the crate root so the
  macOS build stays quiet without hiding real Linux dead code.
Prefer the `cfg_attr` one-liner. Do NOT sprinkle per-item `#[allow(dead_code)]`.

---

## Improvement D — (stretch) `pf-ui-emu` binary: run the menus on macOS

Now genuinely reachable — `ui`/`core`/`services` build on the host, and after A the renderer
no longer needs `App`. This delivers the emulator the whole refactor was aimed at.

1. Add `src/bin/pf-ui-emu.rs`, NOT gated to Linux. Deps it needs (`sdl2` for a plain window,
   or `softbuffer`/`minifb` to avoid the SDL dep on macOS) — prefer a tiny pure-Rust window
   crate so the emulator needs no SDL on the host.
2. Provide a host `TextRaster` impl (the SDL2_ttf one is Linux-only). Either add a
   `fontdue`/`ab_glyph` host implementation of `ui::TextRaster`, or feature-gate a stub. This
   is the main new work — text is the one platform seam `ui` still depends on.
3. Feed fake data: a couple of `core::model::KnownHost`s + `GameEntry`s, no discovery/mTLS.
4. Loop: map host keyboard → `core::MenuEvent`, run `App::update` (state only), then
   `TileCache::prepare`/`compose` → blit the composed framebuffer to the window.
5. Document in `CLAUDE.md`: `cargo run --bin pf-ui-emu` on macOS.

**Prereq:** A must be done (renderer decoupled from `App`) and a host `TextRaster` exists.
Scope this as its own project after A lands.

---

## Suggested sequencing & verification

| Step | Risk | Verify |
| --- | --- | --- |
| A0 separate mutation | med | docker:check + on-device fades |
| A1 group tile fields | low | docker:check + macOS check |
| A2 move render methods | **high** | docker:check + macOS + full on-device screen sweep + screenshot compare |
| B runtime input.rs | low | docker:check + clippy |
| C1 Rect helpers | low | docker:check |
| C2 macOS dead-code | low | macOS `cargo check` (warnings → 0) |
| D emulator | med | `cargo run --bin pf-ui-emu` on macOS |

Every step: `task fmt` + `task docker:lint` before commit. No behavior change — if a screen
looks different on device, revert and re-cut smaller.

## Verification commands
```sh
task docker:check          # Linux compile (shipping target)
task docker:lint           # clippy -D warnings
cargo check --bin punktfunk-webos   # macOS host: platform-independent layers still build
task deploy TV_HOST=root@<tv-ip>    # on-device smoke (no test suite exists)
```
On-device smoke after A: open every screen (Home grid+sidebar, Settings+dropdowns+scroll,
Pairing, Add/Edit host, Host menu, Wake, Wake settings, Forget, About scroll, Speed test,
Diagnostics), confirm focus glow / card pop-in / modal fades / scrolling are pixel-identical,
then stream and confirm the in-stream overlays.
```
