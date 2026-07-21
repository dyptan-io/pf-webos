# Architecture notes and hard-won gotchas

This document captures the non-obvious decisions, platform limitations, and debugging trails
from building this client, so they don't have to be rediscovered. Developed and verified against
a real **LG CX, webOS 5.6**, using root SSH access for logs/testing.

## Memory/performance pass (2026-07-12)

Verified on real hardware (LG CX) — see the soft-float finding below for the pass that actually
moved the needle; the items here are real but each individually minor next to that one.

- **`ui::TextCache`**: `ui::draw_text` used to rasterize (freetype) and upload a brand-new GPU
  texture on *every* call, with zero caching — and every draw function runs on every render tick
  of the ~60fps pre-stream UI loop, so a static label like "Settings" paid that cost 60×/sec for
  pixels that never changed (`draw_highlighted_text`, used for PIN/IP entry, made this worse by
  calling `draw_text` once per character). Keyed by `(font address, text, color)` and reused across
  frames — created once in `main.rs::run_ui_flow`, threaded down through every render call. (Since
  the rendering-backend rewrite below, the cached value is a `tiny_skia::Pixmap`, not a GPU
  texture, and `TextCache::new()` no longer takes a `texture_creator` at all — nothing in `ui.rs`
  ever needed a raw `TextureCreator` for anything past this point.)
- **Redraw-on-change**: the same loop called `app.render(...)` (and its `canvas.present()` vsync
  swap) unconditionally every 16ms tick forever, even sitting on a completely untouched menu. Safe
  to skip when nothing changed *because* this UI has no time-based animation anywhere (no spinner/
  blink/marquee) — every pixel that can change does so only in reaction to an SDL event, a
  Discovery/art background result, or the raw scancode Back/Red edge, all of which now set a
  `dirty` flag that gates the render call.
- ~~**Cover-art GPU texture leak**: `app.art_pixels` (raw RGBA) gets cleared on every host switch,
  but `main.rs`'s separate GPU-texture cache built from it was never pruned to match.~~ Moot since
  the rendering-backend rewrite below: `app.art` (a `HashMap<String, tiny_skia::Pixmap>`) *is* the
  drawable object now, composited straight into the frame `Painter` — there's no second,
  main.rs-owned GPU-texture cache left to fall out of sync with it at all.
- **Cover art decoded at full source resolution**: Steam-CDN-style capsules commonly exceed
  1000px on a side; the grid never draws a card anywhere near that (`ui::CARD_MIN_W` is 220px).
  `art.rs` downscales (aspect-preserved, cap 480px on the longer side) before the `Pixmap` is built.
- **A fresh mTLS handshake per cover-art fetch**: `library::fetch_art` built a brand-new
  `ureq::Agent` (fresh TLS config, re-parsed PEM identity, fresh TCP+TLS handshake with
  client-cert auth) on every call, and `art.rs` calls it once per game — a 30-50 game library paid
  for that many redundant mutual-TLS handshakes to the *same* host. `library::agent` is now public
  so `art.rs` builds one per batch and reuses it across every game's fetch.
- **`App::select_host` used to call `library::fetch_games` directly on the UI/render thread** —
  a real network round-trip (up to `library::agent`'s 5s connect / 10s total timeout), blocking
  *all* input and rendering for as long as the host took to answer or time out. Hit on every app
  launch too (`App::new` restores the last-selected host via the same call). Surfaced as "some
  button presses don't register for 1-2 seconds." Fixed the same way cover art already loads:
  `library::load_games_async` spawns a thread and delivers a `GamesLoaded` over a channel,
  drained each tick by `App::drain_games`. Switching hosts again before a fetch finishes is safe
  — `select_host` replaces `games_rx` with a fresh channel, so the stale thread's send just fails
  and it exits (same pattern `art::load_art_async` already relied on). The pairing PIN ceremony
  (`App::handle_pairing_event`) still blocks the same way — not yet fixed, since it's a rare,
  explicitly user-initiated action rather than something on the startup/host-switch hot path.

## Linting (`task lint`/`task native:lint`, format via `task fmt`)

`Cargo.toml`'s `[lints.clippy]` is a curated slice of `pedantic`/`nursery` lints, not a blanket
`#![warn(clippy::pedantic)]`. Tried the blanket version first: it surfaced ~360 warnings, and over
300 of them were `cast_possible_truncation`/`cast_sign_loss`/`cast_precision_loss`/
`cast_possible_wrap` on the SDL2 rect/color/font pixel-math scattered through `ui.rs`/`app.rs` —
none a real risk (every value involved is bounded by a TV panel's own resolution, nowhere near
`i32`/`u32` limits), and fixing them would mean `try_from`/`#[allow]`-ing hundreds of call sites
for zero actual safety gain. Picked out the lints that were both real and low-noise instead
(`cast_lossless`, `use_self`, `map_unwrap_or`, `doc_markdown`, `manual_let_else`,
`redundant_closure_for_method_calls`, `items_after_statements`, `match_same_arms`,
`format_collect`, `suspicious_operation_groupings`) and left the rest at their default (`clippy::all`)
level. `clippy::cargo` (dependency-version-duplication lints) and `too_many_lines` (main.rs's
event-loop functions) are deliberately not enabled — the former is out of this crate's control
(comes from `punktfunk-core`'s own transitive deps), the latter would force splitting cohesive
state-machine loops with no natural seam, for a line-count threshold alone.

## Toolchain (reproducible via `task toolchain:all` — see `Taskfile.yml`/`taskfiles/toolchain.yml`)

- Cross target: `armv7-unknown-linux-gnueabi` (Rust tier-2) + `webosbrew/native-toolchain`'s
  `arm-webos-linux-gnueabi-gcc` (buildroot, GCC 12.2.0). Only ships a **Linux aarch64** build for
  Linux (no `linux-x86_64` release exists) — so local dev always runs inside the Docker build
  container (`task build`/`check`/`package`, forced to `--platform linux/arm64` so this works the
  same on an amd64 host too, via QEMU emulation). CI runs the `native:*` tasks directly instead,
  since its runner is already Linux aarch64.
- `.cargo/config.toml` wires the linker/CC/pkg-config env vars to `scripts/cc-shim.sh`/
  `cxx-shim.sh`, which pass `--sysroot` explicitly — this toolchain's baked-in default sysroot
  path is stale post-relocate.
- **`armv7-unknown-linux-gnueabi` defaults to real software-emulated floating point, not just a
  soft-float *calling convention*** — this was the actual root cause of a "the whole UI is
  laggy" report that survived several rendering-side fixes (redraw-on-change, shadow/text
  caching, a streaming texture) with zero effect, because none of those touched the real
  bottleneck. Confirmed via `nm`/`objdump` on a release binary: even a near-empty frame (no host
  selected, zero cards) spent ~300ms in `render()`, and disassembly showed basic f32/f64 add/mul
  compiling to calls into `compiler_builtins`/`__aeabi_f*` — software emulation — instead of a
  single VFP instruction. The vendor's own C toolchain targeting this exact chip
  (`arm-webos-linux-gnueabi-gcc -v`) defaults to `-mfloat-abi=softfp -mfpu=neon-fp16
  -mcpu=cortex-a9` — **softfp**, meaning real VFP3/NEON hardware instructions for computation,
  base-AAPCS (integer-register) calling convention only at ABI boundaries — matching a real
  Cortex-A9 FPU the sysroot's own libSDL2 etc. already use. Rust's built-in `gnueabi` (non-`hf`)
  target spec instead bakes in LLVM's `soft-float` feature unconditionally, disabling hardware FP
  codegen even though the platform (and every C object in the same binary) supports softfp fine.
  Fix: `.cargo/config.toml`'s `[target.armv7-unknown-linux-gnueabi]` sets
  `rustflags = ["-C", "target-feature=+neon,+vfp3,-soft-float", "-C", "target-cpu=cortex-a9"]` —
  `-soft-float` only changes *codegen* (real VFP/NEON instructions for computation), not the
  calling convention, so FFI calls into the sysroot's softfp-ABI libraries stay correct. Measured
  effect on-device: ~300ms → ~30ms per render. (`rustc`/`cargo` emit a stable-but-harmless
  "unstable feature" warning for `neon`/`vfp3`/`soft-float` on `-C target-feature=` — real,
  doesn't fail `-D warnings` builds, safe to ignore.) rustup's prebuilt `std`/`core` for this
  target were still built with the old default and can't be overridden without `-Z build-std`
  (nightly) — some soft-float calls remain from there, but the hot rendering path is ours, not
  std's, so this fix is the one that mattered.
- **getauxval/gettid/sendmmsg shims required**: webOS's shipped glibc is ~2.12, predating
  `getauxval()` (2.16+), `gettid()` (2.30+), and `sendmmsg()` (2.14+) — all linked unconditionally
  by Rust std / punktfunk-core's UDP batching. Fixed via `src/glibc_compat_shim.c` (raw
  `syscall(2)` for the latter two, `/proc/self/auxv` parsing for the first) + `build.rs`, linked
  as a bare object via `cargo:rustc-link-arg` — **must land AFTER libstd in the link line**:
  `cargo:rustc-link-lib=static=...` places it too early and a single-pass linker drops it as
  unneeded before libstd's undefined reference even appears.
- **SDL2 must be the webOS-patched fork, not generic**: the NDK's own bundled SDL2 lacks webOS's
  custom Wayland shell-integration protocol (`QT_WAYLAND_SHELL_INTEGRATION=webos`) — its Wayland
  driver reports "not available" even with every env var webOS sets correctly. Fix: overlay
  `webosbrew/SDL-webOS` release `release-2.30.12-webos.5` onto the NDK sysroot, the same
  dependency aurora-tv/moonlight-tv/RetroArch-webOS all bundle. `task toolchain:sdl2` does this
  automatically.
- On-device system libSDL2 is **2.0.10** — far too old (missing ABI symbols like
  `SDL_Metal_DestroyView`). The `.ipk` bundles its own `libSDL2-2.0.so.0` in `lib/` with an
  `$ORIGIN/../lib` RPATH (set in `build.rs`) — never rely on the system copy.
- `punktfunk-core`'s `quic` feature transitively needs `cmake` (via `opus` → `audiopus_sys`
  vendoring libopus). Two gotchas: (1) wire `CMAKE_TOOLCHAIN_FILE_armv7_unknown_linux_gnueabi` in
  `.cargo/config.toml` to the NDK's `share/buildroot/toolchainfile.cmake`; (2) modern CMake
  (≥3.31) refuses vendored libopus's old `cmake_minimum_required` — set
  `CMAKE_POLICY_VERSION_MINIMUM=3.5` (a plain, non-target-scoped env var) when building.

## UI rendering performance, round 2 (2026-07-20)

tiny-skia's general shader/blend pipeline (`fill_rect`/`draw_pixmap`) has a large, roughly fixed
per-call cost on this hardware, independent of what's actually drawn — confirmed twice via on-device
timing logs (same deploy-and-read-the-log loop as the soft-float fix above). `draw_modal_backdrop`'s
full-screen semi-transparent fill cost ~300ms alone; a full-frame cache-layer blit cost ~330-350ms —
*more* than the render it was meant to avoid. Both fixed by bypassing the pipeline entirely for
full-buffer work: `Painter::dim` (a raw per-pixel darken loop) and `Painter::blit_layer`
(`copy_from_slice`). **Never route a full-frame or large-area copy/fill through
`draw_pixmap`/`fill_rect` on this target — use a raw `pixmap.data_mut()` loop or `copy_from_slice`,
and verify with real timing logs rather than assuming a call is cheap.**

Two smaller wins (~15-25% each, real but not dominant): `Painter::draw_pixmap_scaled` uses
`FilterQuality::Nearest` instead of `Bilinear` (avoids `Pattern::push_stages`'s extra interpolation
stages), and `ui::solid_paint` sets `anti_alias = false` (a genuinely separate, cheaper
scan-conversion path in tiny-skia).

`App::render` caches the Home (sidebar+grid) layer while a modal is on top of it
(`home_layer`/`home_dirty`) instead of redrawing it every frame — Home alone cost ~170-190ms,
previously repaid on every Settings frame even though Settings never touches Home's content. A
Home-screen frame still draws straight into `painter` (zero extra cost there); every mutation site
that can change Home's content (`select_host`, `drain_games`, `drain_discovery`, `drain_art`,
`forget_host`) sets `home_dirty` explicitly, rather than inferring it from event types in `main.rs`
(an earlier, inference-based version of this cache was fragile and got reverted).

Two real bugs found alongside the perf work:
- `hover_close` was only ever cleared by modal-screen code, never by `Screen::Home` — hovering or
  clicking a modal's close button, then returning to Home, left it stuck `true` forever, silently
  swallowing every subsequent Home click. Fixed in `handle_mouse_motion`'s `Screen::Home` arm.
- `handle_mouse_click` now re-syncs focus to the click's own `(x, y)` first, rather than trusting
  whatever the last `MouseMotion` left behind — a `MouseButtonDown` can carry a slightly different
  position (the button press itself can jostle the remote), so confirming on stale hover state was a
  real, if smaller, contributor to "sometimes needs two clicks."

Cosmetic: `ui::draw_dropdown_overlay` now draws one shadow for the whole panel instead of every
option row casting its own (used to bleed into the gaps between rows). The blue focus-ring outline
(`ui::draw_focus_ring`) now only draws on game/Desktop grid selection, not sidebar/settings rows —
narrowed per request, not removed outright.

**Not yet done**: `rustup`'s prebuilt `std`/`core` still carry the old default `soft-float` codegen
(the `.cargo/config.toml` fix only affects crates built fresh for this target) — nightly
`-Z build-std` would close that gap but is a bigger toolchain change, worth it only if profiling
still shows cost unexplained by the app's own draw calls.

## Runtime/deploy gotchas (LG CX specifics)

- Homebrew apps install to `/media/developer/apps/usr/palm/applications/<appid>/`; the jailer
  sandbox root is `/var/palm/jail/<appid>/`. **`/tmp` is bind-mounted/shared between the jail and
  the host** — a log file the app writes to `/tmp/foo.log` is readable from the plain host SSH
  shell too.
- `luna-send` **needs a real PTY to print output** over a non-interactive SSH exec — without
  `ssh -tt`, output is silently swallowed even on success. Easy to mistake for a hang.
- Install: `luna-send -i -n 1 -f luna://com.webos.appInstallService/dev/install '{"id":"<appid>","ipkUrl":"/tmp/x.ipk","subscribe":true}'`.
  Launch: `luna-send -n 1 -f luna://com.webos.applicationManager/launch '{"id":"<appid>"}'`.
- **The decisive fix for a black screen despite correct decode**: launch through the real app
  lifecycle (`luna-send .../launch`, running as the jailed uid under SAM), never a raw SSH exec —
  even replicating every env var webOS sets. NDL's hardware punch-through video plane apparently
  only composites for the real SAM-managed foreground app; bypassing the app lifecycle for
  convenience during dev connects/decodes fine but never shows anything on screen.
- No documented way to pass CLI args to a native app through a normal SAM launch — worked around
  with a `$HOME/connect.conf` dev-override file the app reads on startup if present.
- SDL2/Wayland reports `refresh_rate=0` from `SDL_GetCurrentDisplayMode` in some launch contexts;
  a virtual-display host may reject a literal 0Hz request, so clamp to a real default.

## Video decode (NDL DirectMedia)

- Header signatures (`NDL_DirectMediaInit/Load/Unload/Quit`, `NDL_DirectVideoPlay/SetArea/
  SetHDRInfo`) come from `mariotaku/ss4s`. `libNDL_directmedia.so.1` is a real on-device system
  library; the webosbrew NDK's sysroot already ships a link-time-only stub with the same symbols,
  so no device round-trip is needed for a fresh build.
- PTS for `NDL_DirectVideoPlay` is milliseconds since `NDL_DirectMediaLoad`, not wall-clock or the
  host's capture clock.
- Audio is NOT routed through NDL — decode client-side via Opus and play through SDL2/PulseAudio
  instead (see below); `NDL_DIRECTMEDIA_DATA_INFO_T.audio` stays zeroed (tag 0 = none).
- **Multi-resolution fix**: decode dimensions (the negotiated *stream* resolution) and the
  punch-through rectangle (the *physical panel* size) must be decoupled once resolution is
  user-configurable — otherwise a 1080p stream on a 4K panel only punches through the top-left
  quarter of the screen.
- **NTSC framerate correction** (`main.rs`'s `ntsc_correct()`, matching aurora-tv's formula):
  1000/1001 × nominal, applied only to 30/60/120/240, floored to a whole Hz. 60→59, 120→119.
- **Loss recovery is required, not optional**: punktfunk's stream has no periodic IDRs, so
  unrecoverable loss produces reference-missing delta frames NDL *silently conceals* (no decode
  error, just a frozen/garbled picture that never self-heals on its own). `session.rs`'s
  `video_pump` calls `client.note_frame_index()` on every frame (cheap, idempotent, fires a
  throttled RFI request internally on a forward gap) plus a throttled `request_keyframe()`
  backstop when `frames_dropped()` climbs.
- **Freeze-until-reanchor, adapted for NDL**: `note_frame_index`'s forward-gap return and a
  `frames_dropped` climb both arm a `holding` flag in `video_pump`; while held, frames are never
  fed to `ndl.play` at all (so the panel just keeps showing its last rendered picture instead of
  a concealed/corrupted one) until one arrives with `FLAG_SOF` (a real IDR) or
  `USER_FLAG_RECOVERY_ANCHOR` (LTR-RFI's clean single-frame recovery) set. Upstream
  `punktfunk_core::reanchor::ReanchorGate` (added in punktfunk v0.10.0) does the equivalent
  decision assuming a decode/present split every other client has — Linux/Windows FFmpeg, Android
  MediaCodec, Apple VideoToolbox — but `NDL_DirectVideoPlay` (checked against the webOS 5.6 SDK
  sysroot's `NDL_directmedia_v2.h`, the latest API version webOS offers; there's no v3) decodes
  and presents in one opaque call with no hook to decode without displaying, so this client can't
  use `ReanchorGate` as designed and reimplements just the skip-until-reanchor subset directly.
  One real gap versus the shared gate: a host's intra-refresh `USER_FLAG_RECOVERY_POINT` wave
  can't be consumed this way (that healing needs every intervening frame actually decoded, which
  holding skips) — hosts limited to that fallback instead heal via the `frames_dropped` keyframe
  backstop forcing a real IDR, which takes longer than the two-mark intra-refresh path would.
- HDR mastering metadata can change over a session (different content, different mastering
  values) — `video_pump` drains `next_hdr_meta` every frame (non-blocking) and applies whatever
  arrives to NDL, rather than fetching it once at connect time.
- `disconnect_quit()` is called only on a deliberate user "stop" (long-press-Back) — the host
  tears the virtual display down immediately instead of lingering for a reconnect. Every other
  exit path (host ended the session, app quit) leaves the connection to close normally.

## Audio

`opus::MSDecoder` (same channel-layout convention the host's encoder uses), played via
`sdl2::audio::AudioQueue<f32>`. **Important Rust-ownership gotcha**: `AudioQueue`/
`AudioSubsystem` wrap an `Rc` internally, so they're **not `Send`** and can't move into a spawned
OS thread the way video decode does — audio is pumped from the *main thread's* event loop each
tick instead (non-blocking, `Duration::ZERO`).

If audio seems dead: check `pactl list sink-inputs` (is the stream reaching PulseAudio, muted, at
what volume) → `pactl list sinks` (is the *hardware* sink itself unmuted) → a peak-amplitude check
on the decoded PCM before assuming the decode path is broken. On this CX, "no sound" turned out to
be the TV's own physical mute, not a bug — plain SDL2/PulseAudio audio works fine as a native
webOS app; NDL's own audio path was never needed.

## UI

Rendering backend (`ui::Painter`, added 2026-07-12): a `tiny_skia::Pixmap` software
framebuffer — real anti-aliased fills/strokes and box-blurred drop shadows, pure Rust so it
cross-compiles exactly like `image` already did. `App::render` draws every screen into one
`Painter` per dirty tick; `main.rs` uploads the finished buffer to a single persistent SDL2
texture and presents it — one texture/copy per frame, not one per widget/art-cover/text-label
the way the previous hand-rolled per-scanline canvas primitives worked. Cover art (`art.rs`)
and cached text (`ui::TextCache`) are both plain owned `Pixmap`s now too, composited straight
into the frame buffer — no separate GPU-texture cache to keep in sync with them (the old
`art_textures`-vs-`art_pixels` leak-prevention `retain()` dance in `main.rs` is gone; there's
only one cache now). Visually verified on a real LG CX — AA quality, shadow softness, and icon
shapes all render as intended. Per-frame cost on real hardware turned out to be dominated by the
soft-float toolchain issue above, not by anything in this rendering backend itself; see that
entry before assuming a rendering change is needed to fix a performance complaint.

Evaluated and deliberately **not** adopted: moonlight-tv's actual LVGL toolkit (its
`src/app/lvgl` folder — a full retained-mode widget tree, cascading per-state/part styles, flex
layout, focus groups, animations). Bridging real LVGL in via FFI would add a second
cross-compiled C dependency (bindgen-for-arm-webos, on top of an already fragile toolchain — see
below) plus its own display/input driver glue; reimplementing LVGL itself in Rust would be a
multi-month framework project for a UI surface that's 4 screens (Home, Pairing, Settings, Add
host). The actual gap versus moonlight-tv's polish was rendering quality (no AA, hard-edged flat
"shadows"), not a missing widget/layout framework — `tiny-skia` closes that gap directly without
either cost.

Renders with LG's own on-device system font (`/usr/share/fonts/LG_Smart_UI-Regular.ttf`) —
**assume it only reliably covers ASCII**: an earlier attempt at a "⚙ Settings" row using the
U+2699 gear glyph rendered as a broken box. All 10 icons this UI uses (tv, lock, add, close,
settings, monitor, schedule, signal, sun, chevron-down) were originally vector-drawn path math for
exactly this reason, then replaced (2026-07-12) with real glyphs from a bundled, subsetted copy of
Google's Material Icons font (`assets/icons/MaterialIcons-subset.ttf`, Apache 2.0 — provenance,
codepoints, and the `pyftsubset` regeneration command are in `assets/icons/NOTICE.md`). Subsetted
down to ~1.7 KB (from the full font's ~357 KB) since only those 10 glyphs are ever drawn; embedded
via `include_bytes!` (no loose asset to stage/ship alongside the `.ipk`, no runtime path to
resolve) and loaded once through `SDL2_ttf`'s `load_font_from_rwops` (`ui::load_icon_font`) — same
`Font`/`TextCache` machinery real text already used, see `ui::draw_icon`. Loaded at one large fixed
size and downscaled per icon rect via `Painter`'s bilinear `draw_pixmap_scaled`, rather than one
`load_icon_font` call per distinct icon size.

Menu navigation: keyboard arrows/Enter/Escape (matches however the Magic Remote's d-pad mode
surfaces to SDL2) and SDL2 gamepad d-pad/A/B, plus direct numeric entry (the remote's number
buttons are plain SDL2 digit keycodes — type-and-auto-advance like a phone lock screen) and Magic
Remote pointer/mouse support (hover-to-focus, click-to-confirm).

Every non-root screen has a persistent top-left Back button (not a row mixed into a list) — the
same "utility slot before the real list" pattern used for the host-list screen's header Settings
button, and reachable by keyboard/remote via the same Up/Down wraparound as any other row, not
just by mouse.

## Confirmed platform limitations (not app bugs — don't try to "fix" these again)

**Frame rate only paces the stream — it can't change the TV's actual panel refresh rate.**
Confirmed via direct inspection of `webosbrew/SDL-webOS`'s source: `SDL_webOSGetRefreshRate` is
the *only* refresh-rate-related function it exposes, and it's read-only (backed by a read-only
Luna service call, `com.webos.service.config/getConfigs`). There is no `SDL_webOSSetRefreshRate`
or any other documented webOS system API to set panel timing from a native/homebrew app — the
Wayland backend only *receives* `wl_output` mode events, it has no path to request one. aurora-tv
and moonlight-tv both only ever *read* this value (for UI display / internal pacing), never set
it; their own commit history shows abandoned attempts at decoder-side high-framerate workarounds,
not a working refresh-rate switch. The panel's actual scan-out rate is fixed at the system level
(HDMI timing negotiated once, or user-toggled TV settings like TruMotion/Game Optimizer) — outside
any homebrew app's reach. Kodi's webOS port has the same limitation.

**`SDL_WEBOS_ACCESS_POLICY_KEYS_BACK` stops the launcher from opening, but the Magic
Remote's Back/Red buttons still never reach the app as a usable scancode.** `webosbrew/
SDL-webOS`'s `src/video/wayland/SDL_waylandwebos.c` sets a Wayland shell-surface property,
`_WEBOS_ACCESS_POLICY_KEYS_BACK`, gated behind this SDL hint — setting it to `"true"` before
window creation (`run_inner`, via `sdl2::hint::set`) is kept because it's confirmed on-device to
do what it's documented to do: the system launcher no longer opens/kills the app when Back is
pressed (a real, working improvement over the old default). But that's as far as it goes: polling
`SDL_GetKeyboardState`'s raw byte array (the only place `SDL_SCANCODE_WEBOS_BACK = 482` would be
visible at all, since `sdl2`'s vendored `Scancode`/`Keycode` bindings only name scancodes up to
290) shows every other key's scancode changing normally, but Back and Red specifically never flip
— confirmed twice on-device, once via a single-scancode check and once via a full-array scan
logging every changed index. Likely cause (unconfirmed): `Wayland_get_scancode_from_key` in
`SDL_waylandevents.c` only calls the webOS scancode fallback (`SDL_GetWebOSScancode`, the thing
that actually maps the LG remote's raw IR keycodes to `SDL_SCANCODE_WEBOS_*`) from one specific
branch, gated on `!input->keyboard_is_virtual`; if this compositor's remote input reports as a
virtual keyboard, that branch — and the webOS fallback with it — is skipped entirely, and these
two LG-specific keycodes have no xkb keysym either, so SDL drops the whole key event before it's
visible in any form (no scancode, no `Event::KeyDown`, nothing — confirmed via a catch-all logger
on every otherwise-unhandled SDL event too, which also logged nothing for these two keys).
Whatever the exact mechanism, don't re-attempt reading Back/Red via `SDL_GetKeyboardState`,
`Keycode`, or `Scancode` without new evidence — the on-screen close (X) button, keyboard/gamepad
Back mapping (`Escape`/`Backspace`/`AcBack`, or a controller's B button), and the in-stream
long-press-to-disconnect are the only working Back paths. The access-policy hint itself is safe
to leave set (confirmed harmless, and it does genuinely stop the launcher takeover) even though
nothing currently consumes the scancode it was meant to unlock.

**A hidden/unmapped window doesn't receive pointer input.** The stream-time window was `.hide()`n
(since `set_opacity` isn't supported on this Wayland backend) so it wouldn't visually cover the NDL
video plane — this silently broke the Magic Remote pointer → host-mouse forwarding (`mouse.rs`),
since there's no mapped surface left for Wayland to route `MouseMotion`/button events to (keyboard-
style remote-key *polling* still worked while hidden, suggesting webOS routes those by foreground-app
identity rather than surface focus — a different path from pointer routing). aurora-tv (the same NDL
punch-through technique, with its own working pointer support) never hides its window at all — it
stays mapped, cleared fully transparent (`Color::RGBA(0,0,0,0)`) each frame instead so the video
plane shows through underneath. `run_inner` now does the same.

**Two independent cursors, not one out of sync.** Once the pointer reached the host, the visible
cursor still looked "wrong" — moving faster than the physical remote. Cause: webOS draws its own
local cursor (a real SDL2 cursor this fork loads from `/usr/share/im/cursorType*.png`, confirmed via
`SDL_waylandwebos_cursor.c`) tracking the remote directly and instantly; the host draws a *second,
independent* cursor wherever our forwarded `MouseMoveAbs` puts it, over the network, with its own
latency. Two cursors that were never going to stay synced, not one buggy one. Fixed by hiding the
local cursor during a stream (`sdl.mouse().show_cursor(false)`, restored for the menu) so only the
host's own cursor is visible. `mouse.rs`'s `move_event` also applies a `SENSITIVITY` scale (0.55,
centered on the panel's middle) since even with only one cursor visible, unscaled 1:1 absolute
positioning still felt fast — the tradeoff is the true edge pixels need the remote pointer to go
slightly past the panel's own edge to reach.

**Magic Remote color buttons (Red/Green/Yellow/Blue) require raw scancode polling, not the safe
SDL2 event API.** Confirmed: `webosbrew/SDL-webOS` (the fork this client links for Wayland shell
integration) adds `SDL_SCANCODE_WEBOS_RED = 486` / `GREEN = 487` / `YELLOW = 488` / `BLUE = 489`
(translated from the X11 keycode 406, sourced from `/usr/share/X11/xkb/keycodes/lg`) — confirmed
live in moonlight-tv's and webosbrew/RetroArch's own source. Vanilla SDL2 has no such scancode at
all (the press is silently dropped there), and **rust-sdl2's safe `Scancode`/`Keycode` enums don't
cover this fork's custom 486+ range either** — `Scancode::from_i32(486)` returns `None`, so the
value is unrecoverable through the safe event API. The fix (`ui::webos_red_button_down()`) reads
the raw SDL2 keyboard-state array directly (`sdl2::sys::SDL_GetKeyboardState` → `*const u8`,
indexed by raw scancode int) — a level read, so the caller edge-detects the down-transition itself.

## Don't re-add: an in-stream diagnostics overlay

Tried once (a Magic Remote Green-button toggle for an in-stream log/stats overlay) and removed
entirely after it crashed the app on the real CX: toggling `window.show()`/`window.hide()` on the
normally-hidden SDL2 window (hidden during streaming so NDL's punch-through video plane shows
through unobstructed) while NDL's hardware video plane was actively compositing killed the process
silently — no panic, no logged error, just gone from `ps aux`. Almost certainly a native crash
inside the Wayland backend that Rust can't catch. If this is wanted again: treat it as new work,
test any window-visibility change in total isolation first (log immediately before/after each SDL
call), and confirm per-pixel alpha on a freshly-shown window actually composites over NDL's plane
on this compositor at all — whole-window `SDL_SetWindowOpacity` is already confirmed unsupported
here, which doesn't answer the per-pixel question but doesn't inspire confidence either.

## Known gaps / not yet done

- **HDR wiring** is implemented (`video_caps`, static + continuously-updated display metadata,
  per-content `NDL_DirectVideoSetHDRInfo` forwarding) but not yet visually confirmed on a real
  HDR-negotiated session.
- Gamepad in-stream input passthrough (`gamepad.rs`) is wired but not yet interactively verified
  with a real controller during an actual stream (menu navigation via gamepad has been exercised,
  not `GamepadButton`/`GamepadAxis` passthrough mid-session).
- **Magic Remote pointer during an active stream**: currently only usable in this client's own
  menus, never forwarded to the host as mouse/touch input while streaming — worth adding if
  remote-desktop-style pointer control is wanted (the C-ABI guide's absolute-pointer contract —
  `flags = (width << 16) | height` — is the wire shape to target).
