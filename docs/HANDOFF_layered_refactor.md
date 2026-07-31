# Handoff: Layered Architecture Refactor (steps 1-4 of 6)

## Original request

> please implement the plan step by step that opus created in notes

Referring to `docs/REFACTOR_PLAN.md` — a 6-step, single-crate layered-module
refactor (no Cargo workspace split) separating the codebase into:

- **`core/`** — domain logic, no `sdl2`, no `tiny_skia`, no I/O
- **`ui/`** — presentation (`tiny_skia` only, no `sdl2`)
- **`platform/`** — the hardware/OS boundary (traits + webOS impls)
- plus `services/` (portable I/O), `session/` (streaming), `runtime/` (top-level loop)

Goal: the UI can be rendered/tested without any platform code (SDL2, NDL,
Starfish). Hard mechanical success test:
```sh
grep -rn "sdl2"      src/ui  src/core   # must be empty
grep -rn "tiny_skia" src/core           # must be empty
```
No behavior change — pure restructuring, verified via `task check`/`task lint`
after every step (no unit test suite exists for this project).

Mid-session, after step 3, the user was asked whether to continue with the
plan's largest step (step 4, the text/sdl2::ttf seam) or stop; they chose to
continue. After step 4, asked again whether to continue to steps 5-6 or stop;
they chose to **stop here for now**.

## Branch

`refactor/layered-architecture`, 4 commits, one per completed step. `main` is
untouched. Nothing was pushed or deployed to a TV.

## What was done

### Step 1 — move portable services (commit `011c594`)
`git mv` of `store.rs`, `discovery.rs`, `library.rs`, `art.rs`, `wol.rs` into
`src/services/`, with a new `src/services/mod.rs`. All `crate::store` /
`crate::discovery` / etc. import paths repointed to `crate::services::*`.
Pure move, no logic changes.

### Step 2 — extract `core/` domain types (commit `c7707fc`)
New `src/core/{mod,screen,event,model}.rs`:
- `core::screen`: `Screen`, `PairingFocus`, `HomeFocus` (moved out of `app/mod.rs`)
- `core::event`: `MenuEvent` (moved out of `ui/input.rs`)
- `core::model`: `ConnectTarget`, `KnownHost`, `Settings` (+ its enums:
  `VideoBackend`, `CodecPref`, `ColorRangeOverride`, `GamepadType`,
  `LogLevelOverride`), `GameEntry`, `Artwork`

To minimize churn, `services::store` and `services::library` `pub use` these
types back out, and `app/mod.rs` / `ui/input.rs` do the same — so existing
`store::Settings`, `ui::MenuEvent`, etc. call sites kept compiling unchanged.

### Step 3 — sever the render seam (commit `1a5c9f7`)
New `src/ui/render.rs`: `Rect`, `Color`, `TileId`, `DrawCmd`, `DrawList` —
UI-native replacements for `sdl2::rect::Rect`, `sdl2::pixels::Color`, and the
former `compositor::Tile`/`compositor::DrawCmd`. Every draw-command site in
`ui/` and `app/` now uses these instead of the sdl2 types.

`compositor.rs` moved to `platform/webos/compositor.rs` (new `platform/` and
`platform/webos/` modules), renamed `Tile`→`TileId`, `execute`→`present`, and
converts to real `sdl2::rect::Rect`/`sdl2::pixels::Color` only at the
upload/present boundary (`to_sdl_rect`/`to_sdl_color`).

After this step: `sdl2::rect` and `sdl2::pixels::Color` no longer appear
anywhere under `src/ui`. Remaining `sdl2` there was `sdl2::ttf` (text), left
for step 4.

### Step 4 — sever the text seam (commit `cba4031`) — the plan's largest step
New `src/ui/text_raster.rs`: `FontId` enum (`Label`/`Value`/`Title`/`Icon`/
`Caption`) + `TextRaster` trait (`rasterize`/`measure`/`height`) — the
abstraction `ui/` depends on instead of a borrowed `sdl2::ttf::Font`.

New `src/platform/webos/text_sdl.rs`: `SdlTextRaster`, the SDL2_ttf-backed
implementation — owns the five loaded fonts, `load_font`/`load_icon_font`
(moved from `ui/text.rs`), and the `pixmap_from_ttf_surface` glyph-surface
conversion.

`ui::Fonts` (in `ui/text.rs`) changed from holding five borrowed `&Font`
references to holding a `&dyn TextRaster` plus five `FontId` markers. Every
function across `ui/*.rs` and `app/*.rs` that previously took `&Font` (or
read `fonts.label.height()` / `fonts.value.size_of(...)` etc.) now takes
`raster: &dyn TextRaster, font: FontId` and calls `raster.height(font)` /
`raster.measure(font, text)` instead — roughly 80+ call sites, done via a mix
of targeted edits (function signatures, the largest files: `text.rs`,
`tiles.rs`, `rows.rs`, `cards.rs`, `about.rs`, `pairing.rs`, `modal.rs`,
`notification.rs`) and scripted `perl` substitutions for the repetitive
`fonts.X` call-site pattern, each followed by a `cargo check` pass to catch
what the scripts missed or mishandled (a few double-inserted `fonts.raster`
args from multi-font-arg calls like `draw_modal_header` had to be hand-fixed).

Also relocated `ui/input.rs`'s raw SDL2 keyboard/gamepad→`MenuEvent` mapping
to `platform/webos/input.rs` (only `MenuEvent` itself is a domain type; the
SDL mapping is platform) — this was needed to make the `grep -rn sdl2 src/ui`
check actually pass, since the plan's own end-state check (§6) requires it
and step 4's stated check literally says the same grep must be empty.

**Verified after step 4:** `grep -rn sdl2 src/ui src/core` returns only
doc-comment mentions (e.g. "mirrors `sdl2::rect::Rect`") — no `sdl2` type
appears in any signature under `src/ui` or `src/core`. This was the core
UI-testability goal of the whole plan.

## Verification performed each step

- `task -s docker:check` (native `cargo check` isn't available on this
  Darwin-arm64 dev box — no `webosbrew/native-toolchain` release for it — so
  `docker:check` was used throughout instead of bare `check`)
- `task -s docker:lint` (clippy) — a few `clippy::use_self` and
  `clippy::doc_markdown` findings were fixed inline
- `task -s fmt`, then re-verified check+lint stayed green after formatting

**Not done:** no on-device deploy/smoke test. The plan's own §6 checklist
calls for `task deploy TV_HOST=...` and manually exercising every screen —
that should happen before (or instead of) proceeding further, since steps
1-4 have not been run on real hardware yet despite being "no behavior
change" by construction.

## Step 5 — split app state from view (commits `4e8f89c`, `a53d870`)

Done for all 13 former `app/<screen>.rs` modules (`about`, `addhost`,
`diagnostics`, `edithost`, `experimental`, `forget`, `home`, `hostmenu`,
`pairing`, `pinlimit`, `reach`, `sendlogs`, `settings`, `speedtest`, `wake`,
`wakesettings`):

- Logic (`handle_*_event`, `open_*`, state mutation/decision helpers) moved to
  `src/core/state/<screen>.rs`.
- View (`render_*`, `*_rect`/`*_layout` geometry, `ConfirmButton`/`FocusRow`
  building) moved to `src/ui/view/<screen>.rs`.
- Both stay as `impl App` blocks (App itself did not move — see below), so this
  was a pure move: every call site elsewhere in the crate (`main.rs`,
  `app/mod.rs`'s own `draw_list`/`prepare_tiles`/hit-testing) kept compiling
  unchanged. `reach.rs` had no view half (pure background-probe logic); it
  became `core/state/reach.rs` only.
- `GridLayout`'s previously-private fields (`pinned_count`, `desktop_in_rest`,
  `front_count`) had to become `pub(crate)`, since `core::state::home` now
  constructs one from outside the `app` module.

**`core::effect::Effect`** (`src/core/effect.rs`) was added with one variant,
`SetLogOverlay(bool)`, plus an `execute(Effect)` free function that is the one
place allowed to call into `crate::real`/`crate::services`/`crate::session` on
logic's behalf. `handle_diagnostics_event` now returns `Vec<Effect>` instead of
calling `crate::real::set_log_overlay_enabled` inline; `main.rs`'s Diagnostics
dispatch arm executes whatever it returns. This is deliberately the *only*
call site converted so far — see "What's deferred" below for why the rest
weren't, and what the enum should grow into.

### Deviation from the plan: `App` did not move to `core/state/mod.rs`

The plan's target layout has `core/state/mod.rs` own the `App` struct itself
(`"App state; App::update(event) -> Vec<Effect>"`). This session did not do
that: `App` (`src/app/mod.rs`) holds dozens of `tiny_skia`-backed view-tile
cache fields (`sidebar_layer: Option<Painter>`, `card_tiles:
HashMap<String, CardTile>`, `modal_tile`, `ring_tile`, `dropdown_overlay_tile`,
etc. — the rasterized-once GPU-tile cache `prepare_tiles`/`draw_list` manage).
Moving `App` into `core/state` as-is would make `grep -rn tiny_skia src/core`
non-empty, directly failing the plan's own §6 check — the plan doesn't call
out this tension, likely because it wasn't visible until the tile-cache
fields were actually read.

Splitting the view-tile cache out of `App` into its own `ui`-owned type (so
`core::state::App` holds only domain/focus/scroll state, and a separate
`ui::ViewCache` or similar holds the `Painter`/tile fields, wired together by
`runtime`) would resolve this properly — but it's a larger, separate
restructuring of `App`'s own shape, not a mechanical per-screen move, and
wasn't attempted here. `App` and `app/mod.rs`'s cross-screen plumbing
(`draw_list`, `prepare_tiles`, `tick_animations`, mouse hit-testing,
`back()`) stay in `app/mod.rs` for now.

### What's deferred (and why)

- **Effect-wiring the rest of the I/O call sites.** `core/state/*` still calls
  `crate::services::*`/`crate::session::*` directly in several places (see
  `grep -rn "crate::services\|crate::session" src/core` — non-empty,
  deliberately, unlike the `sdl2`/`tiny_skia` checks below). Two shapes show up:
  - **Synchronous, return-value-free** (`store::save_known_hosts`,
    `store::save_selected_host`, `wol::wake_and_log`'s bool result feeding
    immediate state, `art::clear_host_cache`, `logger::latest_log_file`) —
    these could become `Effect` variants fairly mechanically, following the
    `SetLogOverlay` pattern.
  - **Async, receiver-based** (`services::library::load_games_async`,
    `session::request_access`, `session::run_speed_probe`, the log upload
    thread in `sendlogs.rs`) — these spawn a worker and stash its `Receiver` in
    `App` (`games_rx`, `pairing_rx`, `speed_test_rx`, `send_logs_rx`), drained
    on later ticks. Turning these into `Effect`s cleanly needs the runtime to
    both spawn *and* hand the resulting `Receiver` back into `App` state,
    which means either `Effect` carrying a callback/closure, or `App` exposing
    setter methods the runtime calls after spawning. Wiring this without
    behavior risk needs its own careful pass — not attempted here to avoid a
    half-verified change to every background-fetch path in one session.
  - Realistically, finishing this bleeds into step 6 (`session`/`runtime`
    exist as separate modules only after that step).
- **Collapsing `main.rs`'s 16 separate `Screen::X => app.handle_x_event(...)`
  dispatch sites into one `App::update(event) -> Vec<Effect>` entrypoint.**
  Not done — only the Diagnostics arm was touched, to prove the `Effect`
  pattern compiles end to end. The other 15 dispatch arms are unchanged.
  `home`'s handler and `back()` already return `Option<ConnectTarget>`, which
  is its own pre-existing effect-like escape hatch; unifying everything under
  `Vec<Effect>` would fold that in too.
- **The `App`/tile-cache split described above.**

None of this was skipped for lack of importance — it's the harder half of
step 5 (turning "logic doesn't call I/O directly" into "logic doesn't call
I/O directly, full stop"), and doing it for all thirteen screens' worth of
async plumbing in one sitting risked introducing exactly the kind of subtle
behavior change the plan explicitly prohibits. The per-screen logic/view file
split above is the mechanical, low-risk 90% of step 5; what's deferred is the
higher-risk 10%.

## Step 6 (not started) — relocate platform modules + thin `main.rs`

See `docs/REFACTOR_PLAN.md` §5 for full detail on step 5 (now done, see
above) and below for step 6:

- **Step 6 — relocate platform modules + thin `main.rs`.** `git mv` the
  remaining webOS-specific modules (`ndl.rs`, `starfish.rs`, `luna.rs`,
  `device.rs`, `audio.rs`, `gamepad.rs`, `keyboard.rs`, `mouse.rs`,
  `dualsense.rs`, C/C++ shims) into `platform/webos/`, update `build.rs`
  shim paths, move `session.rs`→`session/mod.rs`, and shrink `main.rs`'s
  `mod real` body into a new `runtime/` module (`ui_flow.rs`, `stream.rs`).
  Optionally define `VideoSink`/`AudioSink` traits (lower priority, plan says
  skip if time-boxed).

## Verification performed this session (step 5)

- `task -s docker:check` and `task -s docker:lint` green after the full
  13-screen split + `core/effect.rs` + the Diagnostics dispatch change.
- `task -s fmt`, then re-verified `docker:check`/`docker:lint` stayed green
  after formatting.
- End-state greps re-run: `grep -rn sdl2 src/ui src/core` and `grep -rn
  tiny_skia src/core` are still empty (unaffected by this step). `grep -rn
  "crate::services\|crate::session" src/core` is **not** empty — expected,
  see "What's deferred" above.
- **Not done: any on-device deploy/smoke test.** This session had no TV
  access. Steps 1-5 have never been run on real hardware. Do a full
  `task deploy TV_HOST=...` pass — every screen listed in the plan's §6
  checklist — before merging any of this branch to `main`.

## Notes for whoever picks this up

- Continue on the `refactor/layered-architecture` branch; do not start a new
  one.
- `task -s docker:check` / `task -s docker:lint` / `task -s fmt` is the
  verification loop; there is no automated test suite. Do an actual
  `task deploy TV_HOST=...` on-device smoke test before merging any of this
  to `main`, given the accumulated diff size.
- `docs/REFACTOR_PLAN.md` is the source of truth for step 6's exact shape —
  it hasn't been edited during this session (step 5's actual landing shape,
  including the `App`/tile-cache deviation, is recorded above instead).
- Before starting step 6, consider whether to first finish step 5's deferred
  Effect-wiring (see above) — the plan lists step 6 as lower priority/more
  mechanical, so tackling the remaining Effect work first may be less risky.
