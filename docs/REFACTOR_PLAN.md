# Refactor Plan: Layered Architecture (UI / Core / Platform separation)

**Audience:** an implementer (human or model) doing the work step by step.
**Read this whole document before touching code.** Also read `docs/NOTES.md`
(on-device findings + "don't re-attempt" list) — some constraints there
override the obvious approach.

This is a **single-crate, layered-module refactor**. We are NOT splitting into
a Cargo workspace. We are NOT changing runtime behavior. Every step must keep
the app compiling and behaving identically. This is a pure restructuring.

---

## 1. Goal and success criteria

Separate the codebase into three layers with a strict one-directional
dependency rule, so that **the UI can be rendered and tested without any
platform code (SDL2, NDL, Starfish)**.

The layers, from innermost (no dependencies) to outermost:

1. **`core/`** — domain logic. Pure data + state machine. No `sdl2`, no
   `tiny_skia`, no I/O, no threads talking to hardware.
2. **`ui/`** — presentation. Turns domain state into a list of draw commands
   using the `tiny_skia` software rasterizer. **No `sdl2`.**
3. **`platform/`** — the hardware/OS boundary, expressed as traits plus their
   webOS implementations (SDL2 windowing, NDL/Starfish video, audio, input).
4. Plus supporting layers: `services/` (portable I/O), `session/` (streaming
   orchestration), `runtime/` (the top-level loop that wires everything).

**Dependency rule (must hold at every step):**

```
runtime   → everything
platform  → core, ui
session   → core, platform(traits)
services  → core, platform(traits)
ui        → core
core      → (nothing in this crate)
```

**The one hard, mechanically-checkable success test:**

```sh
# After the refactor these MUST print nothing:
grep -rn "sdl2"      src/ui  src/core
grep -rn "tiny_skia" src/core
```

Today those greps return ~50 hits in `src/ui` and the domain types are mixed
into `src/app`. When they return nothing, the separation is done.

**Behavioral success test (unchanged app):**

```sh
task check              # native cargo check, must pass
task deploy TV_HOST=... # deploy to a real TV, menus + streaming still work
```

There is no unit-test suite. Verification is `cargo check` + on-device.

---

## 2. Current architecture (what exists today)

Everything is flat modules under `src/`, gated `#[cfg(target_os = "linux")]`
in `main.rs` (webOS and Linux dev boxes report Linux; macOS/Windows get a stub
`bail!`). Key files and sizes:

| File | Lines | Role |
| --- | --- | --- |
| `src/main.rs` | 2076 | **God file.** SDL init, event pump, `run_ui_flow` (menus), `run_inner` (streaming), session orchestration, input dispatch. All inside `mod real`. |
| `src/app/mod.rs` | 3003 | **God file.** Screen state machine + view-model + `draw_list` (builds draw commands). State and drawing fused. |
| `src/app/*.rs` | ~200–660 ea | One module per screen (home, settings, pairing, hostmenu, addhost, edithost, wake, wakesettings, forget, about, speedtest, diagnostics, experimental, pinlimit, sendlogs, reach). Each mixes event-handling (logic) with rect-geometry (view). |
| `src/ui/*.rs` | ~30–680 ea | Drawing primitives: `painter` (tiny_skia), `text` (SDL2_ttf glyphs), `theme`, `tiles`, `rows`, `sidebar`, `cards`, `grid`, `modal`, `scroll`, `fade`, `notification`, `listmodal`, `animation`, `input` (SDL→MenuEvent), `settings`, `about`, `pairing`. |
| `src/compositor.rs` | 242 | SDL2 texture cache + present. `Tile` enum (texture keys), `DrawCmd` enum (draw ops, uses `sdl2::rect::Rect` + `sdl2::pixels::Color`), `Compositor::{upload, execute, ...}`. |
| `src/session.rs` | 1317 | Streaming orchestration on `punktfunk-core`; video pump thread. |
| `src/ndl.rs`, `src/starfish.rs`, `src/luna.rs`, `src/device.rs`, `src/audio.rs` | | webOS-specific: NDL DirectMedia video, Starfish media pipeline, Luna service bus, device info, Opus audio. Link webOS system libs. |
| `src/gamepad.rs`, `src/keyboard.rs`, `src/mouse.rs`, `src/dualsense.rs` | | SDL2 input handling. |
| `src/store.rs`, `src/discovery.rs`, `src/library.rs`, `src/art.rs`, `src/wol.rs` | | Portable services: JSON persistence, mDNS, mTLS game library, cover-art fetch/decode, wake-on-LAN. |
| `src/logger.rs`, `src/errors.rs`, `src/pacing.rs` | | Cross-cutting utilities. |
| `src/glibc_compat_shim.c`, `src/starfish_c_shim.cpp` | | C/C++ shims compiled by `build.rs` for the webOS cross target only. |

### 2.1 Rendering data flow (important — understand before step 3)

```
App state  --App::draw_list()-->  Vec<DrawCmd>   (in app/mod.rs:2486)
                                       |
ui painters (draw_text, draw_card, ...) rasterize into a tiny_skia Painter
                                       |
Compositor::upload(Tile, &Painter)  -> uploads Painter's pixmap to an SDL texture, cached by Tile
Compositor::execute(canvas, &[DrawCmd]) -> copies cached textures to the SDL canvas per DrawCmd
```

- `Painter` (`ui/painter.rs`) is **pure `tiny_skia`** — this is already clean.
- `DrawCmd` and `Tile` (`compositor.rs`) carry **`sdl2::rect::Rect` and
  `sdl2::pixels::Color`** — this is the leak.
- `Tile::ScrollIndicator(Screen)` and `Tile::ScrollContent(Screen)` embed the
  domain `Screen` enum into the platform texture key.

### 2.2 Text rendering (important — this is the biggest single obstacle)

`ui/text.rs` does NOT rasterize glyphs with `tiny_skia`. It uses **SDL2_ttf**:

```
draw_text(painter, text_cache, font, text, x, y, color)
  -> TextCache::get_or_create(font, text, color)
       -> font.render(text).blended(color)     // sdl2::ttf::Font, sdl2::pixels::Color
       -> pixmap_from_ttf_surface(&surface)     // SDL surface -> tiny_skia::Pixmap
  -> painter.draw_pixmap(x, y, pixmap)
```

- `Fonts` (`ui/text.rs:24`) is a bundle of borrowed `sdl2::ttf::Font`
  references. It has **82 call sites** across `app/*` and `ui/*`.
- `Fonts` is used both for **rasterization** (`font.render`) and **measurement**
  (text width/height queries used for layout).
- Font colors are `sdl2::pixels::Color`.

Making `ui/` free of `sdl2` therefore requires putting glyph rasterization
behind a trait (see step 4). This is the largest mechanical change in the whole
plan.

### 2.3 Input flow

`ui/input.rs` maps raw SDL2 keycodes / controller buttons → `MenuEvent`
(the abstract UI event). `MenuEvent` is defined in `ui/` today but is a domain
concept. The SDL mapping is platform; the enum is core.

### 2.4 App-logic / platform coupling

`app/*` modules call `crate::store`, `crate::discovery`, `crate::session`,
`crate::library`, `crate::art`, `crate::wol` directly (8 modules do). So the
screen state machine is entangled with I/O. `main.rs`'s `run_ui_flow` reads the
`MenuEvent` result and directly spawns `session::connect`.

---

## 3. Target module layout

Create these directories under `src/`. Move files as indicated. Keep module
names; only their location and their imports change.

```
src/
  main.rs                # THIN. cfg dispatch to runtime::run(). ~30 lines.

  runtime/               # top-level loop (was main.rs's `mod real`)
    mod.rs               # owns platform handles; drives core; executes Effects
    ui_flow.rs           # menu loop      (was run_ui_flow)
    stream.rs            # streaming loop  (was run_inner)

  core/                  # DOMAIN. No sdl2. No tiny_skia. No I/O.
    mod.rs
    model.rs             # Host/KnownHost, Identity, Settings, GameEntry, ConnectTarget
    screen.rs            # Screen enum + per-screen focus enums
    event.rs             # MenuEvent, InputEvent (moved out of ui/input.rs)
    effect.rs            # Effect enum: Connect, Persist, Discover, Fetch, Quit, ...
    state/               # the app reducer, split per screen
      mod.rs             # App state; App::update(event) -> Vec<Effect>
      home.rs pairing.rs settings.rs addhost.rs edithost.rs wake.rs ...
                         # (logic ONLY: event handling + state transitions. NO geometry, NO draw.)

  ui/                    # PRESENTATION. tiny_skia only. NO sdl2.
    mod.rs
    render.rs            # NEW: ui-native Rect, Color, TileId, DrawList, DrawCmd
    text.rs              # draw_text over a TextRaster trait (not sdl2::ttf)
    painter.rs theme.rs tiles.rs rows.rs sidebar.rs cards.rs grid.rs
    modal.rs scroll.rs fade.rs notification.rs listmodal.rs animation.rs
    view/                # NEW: (core state) -> DrawList  (was draw_list + the draw halves of app/*.rs)
      mod.rs home.rs settings.rs pairing.rs about.rs addhost.rs ...

  platform/              # THE SEAM + webOS impls
    mod.rs               # traits only: Presenter, InputSource, TextRaster,
                         #              VideoSink, AudioSink, Clock, Storage
    webos/               # cfg(target_os = "linux")
      mod.rs
      compositor.rs      # translate ui::DrawList -> SDL textures + present (was compositor.rs)
      input.rs           # SDL events -> core::InputEvent (was ui/input.rs mapping)
      text_sdl.rs        # SDL2_ttf implementation of TextRaster
      video/ ndl.rs starfish.rs luna.rs
      audio.rs device.rs
      gamepad.rs keyboard.rs mouse.rs dualsense.rs
      # C shims stay here; build.rs paths updated
    stub/                # cfg(not(target_os = "linux")): today's bail! stub

  services/              # portable I/O
    store.rs discovery.rs library.rs art.rs wol.rs

  session/               # streaming orchestration on punktfunk-core
    mod.rs               # was session.rs
    pacing.rs

  logger.rs errors.rs    # cross-cutting; leave near root
```

---

## 4. New abstract types to introduce (exact sketches)

These replace the `sdl2` types that currently leak into `ui/`. Put them in
`src/ui/render.rs`. Keep them dumb (plain data, `Copy` where possible) so both
`ui/` and `platform/` can use them without depending on each other's guts.

```rust
// src/ui/render.rs

/// Integer rectangle. Mirrors sdl2::rect::Rect's (x, y, w, h) shape so the
/// webOS compositor can convert 1:1. x/y signed, w/h unsigned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect { pub x: i32, pub y: i32, pub w: u32, pub h: u32 }

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self { Self { x, y, w, h } }
    // add whatever helpers the current code used from sdl2::rect::Rect
    // (contains_point, right(), bottom(), center(), ...). Port each on demand.
}

/// Straight-alpha RGBA8. Mirrors sdl2::pixels::Color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }

/// Texture cache key. Same variants as today's compositor::Tile, but this now
/// lives in ui/ and carries core::Screen (a plain enum), not a platform type.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TileId {
    Sidebar, FocusRow, Card(String), Ring, CardOutline, PinBadge,
    Modal, ModalFocusElement, DropdownOverlay, DropdownFocusOption,
    Status, NoHost,
    ScrollIndicator(crate::core::screen::Screen),
    ScrollContent(crate::core::screen::Screen),
    ScrollFade, ScrollFadeTop, SpinnerFrame(usize),
    StatsOverlay, Notification, LogOverlay,
    DisconnectDialog, DisconnectFocusButton,
}

/// One composition step, in paint order. Same as today's compositor::DrawCmd
/// but with ui-native Rect/Color/TileId.
pub enum DrawCmd {
    Tex        { tile: TileId, dst: Rect, alpha: u8 },
    TexCropped { tile: TileId, src: Rect, dst: Rect, alpha: u8 },
    Fill       { rect: Rect, color: Color },
}

pub type DrawList = Vec<DrawCmd>;
```

The webOS compositor (`platform/webos/compositor.rs`) keeps its `HashMap<TileId,
sdl2::render::Texture>` cache and converts at the boundary:

```rust
fn to_sdl_rect(r: ui::render::Rect) -> sdl2::rect::Rect {
    sdl2::rect::Rect::new(r.x, r.y, r.w, r.h)
}
fn to_sdl_color(c: ui::render::Color) -> sdl2::pixels::Color {
    sdl2::pixels::Color::RGBA(c.r, c.g, c.b, c.a)
}
```

### 4.1 The platform traits

```rust
// src/platform/mod.rs

/// Presents a finished frame. webOS impl = the SDL compositor.
pub trait Presenter {
    fn upload(&mut self, tile: ui::render::TileId, painter: &ui::Painter) -> anyhow::Result<()>;
    fn upload_raw(&mut self, tile: ui::render::TileId, w: u32, h: u32, rgba: &[u8]) -> anyhow::Result<()>;
    fn drop_tile(&mut self, tile: ui::render::TileId);
    fn clear_all(&mut self);
    fn present(&mut self, cmds: &ui::render::DrawList) -> anyhow::Result<()>;
}

/// Rasterizes text. webOS impl = SDL2_ttf. This is what frees ui/ from sdl2::ttf.
pub trait TextRaster {
    /// Rasterize one line to a premultiplied tiny_skia::Pixmap.
    fn rasterize(&self, font: FontId, text: &str, color: ui::render::Color)
        -> anyhow::Result<tiny_skia::Pixmap>;
    /// Measure without rasterizing (layout needs this — see ui/text.rs width queries).
    fn measure(&self, font: FontId, text: &str) -> (u32, u32);
}

/// Opaque handle replacing the borrowed sdl2::ttf::Font in `Fonts`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FontId { Label, Value, Title, Icon, Caption }

/// Yields abstract input events from the OS. webOS impl wraps the SDL event pump.
pub trait InputSource {
    fn poll(&mut self) -> Vec<core::event::InputEvent>;
}
```

`VideoSink` / `AudioSink` traits: extract the interface `session.rs` needs from
NDL/Starfish/audio. Do this LAST (step 6); it is orthogonal to the UI work and
lower priority. If time-boxed, session/video can keep calling the webOS modules
directly for now — the UI separation is the primary goal.

---

## 5. Step-by-step execution

Do the steps **in order**. After each step: `task check` must pass, and the
step's own check must pass. Commit after each step (small reviewable commits).
Do NOT combine steps.

### Step 0 — baseline

```sh
task check          # confirm green before starting
git switch -c refactor/layered-architecture
```

Record the current `grep -rc sdl2 src/ui | ...` count so you can watch it fall.

### Step 1 — move portable services (lowest risk, pure move)

1. `mkdir src/services`. `git mv` these into it:
   `store.rs discovery.rs library.rs art.rs wol.rs`.
2. Add `mod services;` with submodules, or make `src/services/mod.rs` that
   `pub mod store; pub mod discovery; ...`.
3. Fix import paths: `crate::store` → `crate::services::store`, etc. There are
   ~8 app modules plus main/session referencing these. Compiler lists every one.
4. `task check`.

**No logic changes. This is a rename + re-path only.**

### Step 2 — extract `core/` domain types

1. `mkdir src/core`. Create `core/mod.rs`.
2. **`core/screen.rs`**: move the `Screen` enum and per-screen focus enums out
   of `app/mod.rs`. Update all references to `crate::core::screen::Screen`.
3. **`core/event.rs`**: move `MenuEvent` and `InputEvent` out of `ui/input.rs`
   (leave the SDL→event *mapping functions* in `ui/input.rs` for now; only the
   enums move).
4. **`core/model.rs`**: move plain domain structs — `ConnectTarget`
   (`app/mod.rs:216`), and the pure data types from `store.rs` (`KnownHost`,
   `Settings`, `Identity`) and `library.rs` (`GameEntry`). Leave the I/O
   functions in `services/`; only the data structs move to `core`. `services`
   then depends on `core` for these types.
5. `task check`.

**Watch for:** `Tile::ScrollIndicator(Screen)` in `compositor.rs` now points at
`core::screen::Screen`. Fine — update the path.

### Step 3 — sever the render seam (sdl2 out of draw commands)

This is where `grep sdl2 src/ui` starts dropping toward zero.

1. Create `src/ui/render.rs` with the `Rect`, `Color`, `TileId`, `DrawCmd`,
   `DrawList` types from §4.
2. In `ui/*` and `app/*`, replace every `sdl2::rect::Rect` with
   `ui::render::Rect` and every `sdl2::pixels::Color` with `ui::render::Color`.
   - `Painter` (`ui/painter.rs`) takes `Rect`/`Color` on its public methods —
     switch those signatures to the new types. Internally `Painter` may still
     build `tiny_skia` primitives; only its public param types change.
   - This touches all 13 `app/*` files and most `ui/*` files (they all use
     `rect::Rect`). Port `Rect` helper methods as the compiler flags missing
     ones (`.right()`, `.contains_point()`, etc.).
3. Move `compositor.rs` → `platform/webos/compositor.rs`. Rename its `Tile`
   uses to `ui::render::TileId`, its `DrawCmd` uses to `ui::render::DrawCmd`.
   Add `to_sdl_rect`/`to_sdl_color` converters (§4). `upload`/`execute` bodies
   otherwise unchanged. Rename `execute` → `present` to match the `Presenter`
   trait, and `impl Presenter for Compositor`.
4. `App::draw_list` (`app/mod.rs:2486`) now returns `ui::render::DrawList` with
   no sdl types. `runtime` passes it to `Presenter::present`.
5. Check: `grep -rn "sdl2::rect\|sdl2::pixels" src/ui src/app` → should now be
   empty. `task check`.

### Step 4 — sever the text seam (sdl2::ttf out of ui/) — LARGEST STEP

Go slowly. 82 `Fonts` call sites.

1. Add `TextRaster` trait + `FontId` enum to `platform/mod.rs` (§4.1).
2. Create `platform/webos/text_sdl.rs`: a `SdlTextRaster` holding the
   `sdl2::ttf` context + the five loaded `Font`s (moved from `ui/text.rs`
   `load_font`/`load_icon_font`). Implement `rasterize` (the current
   `font.render(text).blended(color)` + `pixmap_from_ttf_surface` path) and
   `measure` (the current width/height query path).
3. In `ui/text.rs`: change `draw_text` and friends to take
   `&dyn TextRaster` + `FontId` instead of `&Fonts`/`&sdl2::ttf::Font`. The
   `TextCache` stays in `ui/` (keyed by `(text, color, FontId)` now).
4. Replace the `Fonts` struct usage everywhere: the 82 sites currently pass
   `fonts.label` / `fonts.title` etc. → pass `FontId::Label` / `FontId::Title`
   and thread a `&dyn TextRaster` down. Mechanical but wide — let the compiler
   drive it, one error at a time.
5. Check: `grep -rn "sdl2" src/ui` → empty. `grep -rn "tiny_skia" src/core` →
   empty. `task check`.

**Gotcha:** measurement must stay byte-exact or layout shifts. Keep using the
same SDL2_ttf metric calls inside `measure`; do not swap font engines.

### Step 5 — split app state (logic) from view (drawing)

`app/mod.rs` (3003 lines) and each `app/*.rs` currently interleave event
handling (logic) with rect geometry + draw calls (view).

1. For each screen module, split into two:
   - **logic** → `core/state/<screen>.rs`: `handle_*_event`, state mutation,
     which `Effect`s to emit. No `Rect`, no `Painter`, no draw.
   - **view** → `ui/view/<screen>.rs`: geometry (`*_rect` functions) + the
     draw calls that build the `DrawList` for that screen.
2. Introduce `core/effect.rs` with an `Effect` enum. Replace direct
   `crate::services::*` / `crate::session::*` calls inside app logic with
   *returning* an `Effect`. `runtime` executes the effects:
   ```rust
   enum Effect {
       Connect(core::model::ConnectTarget),
       PersistSettings(core::model::Settings),
       StartDiscovery, StopDiscovery,
       FetchLibrary(/*host*/), Quit, /* ... */
   }
   ```
   `App::update(event) -> Vec<Effect>`.
3. `runtime/ui_flow.rs` loop: read `InputSource` → map to `MenuEvent` →
   `app.update()` → execute returned `Effect`s (spawn connect, persist, etc.) →
   `ui::view::draw(&app) -> DrawList` → `Presenter::present`.
4. `task check`; deploy and click through every screen.

**This is the second-largest step.** Do one screen at a time; keep the old code
path working until each screen is fully moved. Start with a small screen
(`about`, `forget`, `pinlimit`) to establish the pattern before `home`/`settings`.

### Step 6 — relocate platform modules + thin main.rs

1. `git mv` webOS modules into `platform/webos/`: `ndl.rs starfish.rs luna.rs
   device.rs audio.rs gamepad.rs keyboard.rs mouse.rs dualsense.rs` and the
   C/C++ shims. Update `build.rs` paths for the shims (`src/glibc_compat_shim.c`
   → `src/platform/webos/glibc_compat_shim.c`, likewise the cpp). Keep the
   `#[cfg(target_os = "linux")]` gating at the `platform` module boundary rather
   than per-file.
2. Move `session.rs` → `session/mod.rs`, `pacing.rs` → `session/pacing.rs`.
3. Move `main.rs`'s `mod real` body into `runtime/`. `run_ui_flow` →
   `runtime/ui_flow.rs`, `run_inner` → `runtime/stream.rs`. `main.rs` shrinks to
   the `cfg` dispatch + `ABR_PROBE_KBPS` env setup + `runtime::run()`.
4. (Optional, lower priority) Define `VideoSink`/`AudioSink` traits and make
   `session` depend on the traits instead of the concrete webOS modules. Skip if
   time-boxed — it does not affect the UI-separation goal.
5. Final checks (see §6). Deploy, full smoke test.

---

## 6. Final verification checklist

```sh
grep -rn "sdl2"      src/ui  src/core     # MUST be empty
grep -rn "tiny_skia" src/core             # MUST be empty
grep -rn "crate::services\|crate::session" src/core  # MUST be empty (logic emits Effects)
task check                                # native cargo check green
task lint                                 # clippy clean (see CLAUDE.md lint list)
task fmt                                  # formatted
task deploy TV_HOST=root@<tv-ip>          # on real hardware:
```

On-device manual smoke (there is no automated test): open every screen (Home
grid + sidebar, Settings + dropdowns + scrolling, Pairing PIN entry, Add host,
Edit host, Host menu, Wake, Wake settings, Forget host, About scroll, Speed
test, Diagnostics), confirm focus animation, scrolling, and modal fades look
identical; then connect a stream and confirm video + audio + the in-stream
overlays (stats, notification, log overlay, disconnect dialog) still work.

---

## 7. Rules, gotchas, do-nots

- **Behavior must not change.** This is a move/rename/retype refactor. If you
  find a bug, note it; do not fix it in the same commit.
- **Read `docs/NOTES.md`.** It lists things already tried and rejected around
  decode, rendering perf, toolchain, and input. Do not re-derive.
- **Do not swap the font engine.** Text stays SDL2_ttf behind `TextRaster`;
  changing rasterizers will shift every layout metric.
- **Keep `unsafe_textures` invariants.** The compositor detaches textures from
  the SDL `TextureCreator` lifetime and must `destroy()` each exactly once (see
  `clear_all`/`drop_tile`). Preserve this when moving the file.
- **Premultiplied vs straight alpha.** `compositor::upload` converts
  premultiplied→straight for non-opaque tiles; `Tile::Sidebar` is the one opaque
  tile. Preserve this exactly (`ui/text.rs` and `art.rs` also rely on RGBA32
  byte order).
- **Comments:** per `CLAUDE.md`, only WHY comments for non-obvious invariants.
  Do not add comments restating moved code.
- **Commits/PRs:** do not add assistant co-author trailers (per user global
  rules). One commit per step; each compiles.
- **Order matters.** Steps 1→2→3→4→5→6. Steps 3 and 4 unblock the sdl2-free UI;
  step 5 unblocks testable app logic. Do not start 5 before 3+4 are green.
- **`task -s`** to suppress command echo when running tasks (saves noise).
- Native `cargo check` uses a stub on macOS/Windows (`main()` bails). The real
  compile target is Linux-aarch64 (how CI runs) or the armv7 webOS cross
  target. `task check` handles this.

---

## 8. Why this order and shape (rationale for judgment calls)

- **Services first (step 1):** lowest coupling, pure move, establishes the
  directory pattern with near-zero risk.
- **Core types before UI seams (step 2):** `ui/render.rs`'s `TileId` needs
  `core::Screen`; the render seam can't be clean until `Screen` has moved.
- **Render seam before text seam (3 before 4):** independent, but the render
  seam is smaller and proves the "ui-native types + platform converts at the
  edge" pattern before you apply the same idea to the harder text case.
- **State/view split last among UI work (step 5):** it depends on both seams
  being clean, and it is the most invasive; doing it on an already-decoupled
  render/text layer is far safer.
- **Platform relocation + traits last (step 6):** moving webOS files is
  mechanical and the `VideoSink`/`AudioSink` abstraction is orthogonal to the
  UI-separation goal, so it carries the least urgency and the most build-system
  risk (C shim paths, `build.rs`).
```
