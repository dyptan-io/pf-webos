# Architecture notes and hard-won gotchas

Non-obvious decisions, platform limits, and debugging trails, so they aren't rediscovered.
Developed and verified against a real **LG CX, webOS 5.6** (root SSH for logs/testing). Dated debug
narratives have been pruned; what remains is the load-bearing current state.

## Toolchain (reproducible via `task toolchain:all`)

- Cross target `armv7-unknown-linux-gnueabi` (Rust tier-2) + `webosbrew/native-toolchain`'s
  `arm-webos-linux-gnueabi-gcc` (buildroot, GCC 12.2.0). It ships **Linux-aarch64-only**, so local
  dev runs in Docker (`--platform linux/arm64`, works on amd64 via QEMU); CI runs `native:*` directly.
- `.cargo/config.toml` wires linker/CC/pkg-config to `scripts/cc-shim.sh`/`cxx-shim.sh`, which pass
  `--sysroot` explicitly (the toolchain's baked-in default is stale post-relocate).
- **Soft-float was the single biggest perf fix (~300ms → ~30ms per render).**
  `armv7-unknown-linux-gnueabi` defaults to *software-emulated* FP, not just a soft-float ABI —
  Rust's non-`hf` target spec bakes in LLVM's `soft-float` feature, disabling hardware FP codegen
  even though the platform (softfp, real VFP3/NEON) supports it. Fix in `.cargo/config.toml`:
  `target-feature=+neon,+vfp3,-soft-float` + `target-cpu=cortex-a9`. `-soft-float` changes *codegen*
  only, not the calling convention, so FFI into softfp-ABI sysroot libs stays correct. (The
  "unstable feature" warning it emits is harmless. rustup's prebuilt `std`/`core` still carry the old
  default — closing that needs nightly `-Z build-std`, not yet done; the hot path is our code, not std.)
- **glibc shims required**: webOS's glibc is ~2.12, predating `getauxval`/`gettid`/`sendmmsg`.
  Backfilled in `src/glibc_compat_shim.c` + `build.rs`, linked via `cargo:rustc-link-arg` and
  **must land AFTER libstd in the link line** (`link-lib=static` places it too early; single-pass
  linker drops it before libstd's undefined ref appears).
- **SDL2 must be `webosbrew/SDL-webOS`** (`release-2.30.12-webos.5`), not generic SDL2 — only the
  fork has webOS's Wayland shell-integration (`QT_WAYLAND_SHELL_INTEGRATION=webos`). On-device system
  SDL2 is 2.0.10 (too old). The `.ipk` bundles its own `libSDL2-2.0.so.0` in `lib/` with an
  `$ORIGIN/../lib` RPATH (set in `build.rs`) — never rely on the system copy. `task toolchain:sdl2`
  overlays it automatically.
- **cmake/opus cross-build**: `punktfunk-core`'s `quic` feature needs `cmake` (via `opus` →
  `audiopus_sys`). (1) wire `CMAKE_TOOLCHAIN_FILE_armv7_unknown_linux_gnueabi` in `.cargo/config.toml`
  to the NDK's `toolchainfile.cmake`; (2) set `CMAKE_POLICY_VERSION_MINIMUM=3.5` (modern CMake refuses
  vendored libopus's old `cmake_minimum_required`).

## Linting

`Cargo.toml`'s `[lints.clippy]` is a curated slice of `pedantic`/`nursery`, **not** blanket
`clippy::pedantic` — the blanket version surfaced ~360 warnings, 300+ of them harmless `cast_*` on
resolution-bounded pixel math. `clippy::cargo` (out of our control — from `punktfunk-core`'s deps)
and `too_many_lines` (would split cohesive event-loop state machines) are deliberately off.

## UI rendering

Backend is a **hybrid rasterize-CPU / composite-GPU** design. `ui::Painter` (tiny-skia software
framebuffer) rasterizes each widget into standalone cached *tiles* owning a GPU texture in
`compositor.rs`; `App::prepare_tiles` re-rasterizes only stale tiles; `App::draw_list` emits
per-frame texture-copy commands the `opengles2` SDL renderer runs. Position/scroll/scale/fade are
dst-rect/alpha params, so pure animation/nav frames cost ~zero CPU (enables the 60fps eased scroll,
focus pop, modal fade). Redraw-on-change: `tick_animations` keeps frames flowing while animating;
only `content_dirty` frames re-rasterize.

Key hardware facts driving this design:
- **Never route a full-frame or large-area copy/fill through `draw_pixmap`/`fill_rect` on this
  target** — tiny-skia's general pipeline has a large fixed per-call cost here (a full-screen fill
  cost ~300ms). Use a raw `pixmap.data_mut()` loop or `copy_from_slice` (`Painter::dim`,
  `Painter::blit_layer`), and verify with real on-device timing logs, never assume a call is cheap.
- Tiles are premultiplied-alpha — `Compositor::upload` un-premultiplies (SDL's `BlendMode::Blend`
  expects straight alpha). `SDL_RENDER_SCALE_QUALITY=1` (linear) is required or the focus pop shimmers.
  The sdl2 crate's `unsafe_textures` feature lets the texture cache live in a struct.
- `FilterQuality::Nearest` (not Bilinear) and `anti_alias=false` in `ui::solid_paint` are each
  cheaper scan-conversion paths in tiny-skia.

If profiling is needed again, re-add the "render: Xms" log line in `run_ui_flow` — on-device timing
is the only ground truth. tiny-skia was evaluated as a possible dead end and kept: the cost was
re-pushing widgets through its pipeline per frame, not rasterization per se; alternatives (per-widget
SDL textures, real LVGL, GLES2-direct) all lose for a 4-screen menu.

**Fonts/assets**: text uses punktfunk's brand font **Geist** (Regular/Medium/SemiBold/Bold OTFs from
`pf-console-ui`, embedded via `include_bytes!`, `ui::load_font`). Brand palette: dark `#1c1530`,
primary purple `#6c5bf3`, lavender `#a79ff8`, pale-lavender `#d2c9fb`. Sidebar header + splash come
from the real logo artwork (`assets/logo/`). **Assume fonts cover Latin only** — the 10 UI icons come
from a subsetted Material Icons font (`assets/icons/MaterialIcons-subset.ttf`, ~1.7 KB, embedded),
not the text font (a U+2699 gear in the text font rendered as a broken box).

Navigation: keyboard arrows/Enter/Escape, SDL2 gamepad d-pad/A/B, numeric entry (remote number
buttons = plain digit keycodes), and Magic Remote pointer (hover-to-focus, click-to-confirm). Every
non-root screen has a persistent top-left Back button, reachable by remote via Up/Down wraparound.

## Video decode (NDL DirectMedia)

- Header signatures from `mariotaku/ss4s`. `libNDL_directmedia.so.1` is a real device library; the
  NDK sysroot ships a link-time stub, so no device round-trip is needed to build.
- PTS is milliseconds since `NDL_DirectMediaLoad`, not wall-clock/host capture clock.
- Audio is **not** routed through NDL — decoded client-side via Opus, played through SDL2/PulseAudio.
- **Decouple decode dimensions (negotiated stream res) from the punch-through rect (physical panel
  size)** — otherwise a 1080p stream on a 4K panel punches through only the top-left quarter.
- **NTSC framerate correction** (`ntsc_correct()`): 1000/1001 × nominal, only for 30/60/120/240,
  floored (60→59, 120→119).
- **Loss recovery is required**: punktfunk's stream has no periodic IDRs, so unrecoverable loss
  produces reference-missing frames NDL *silently conceals* (frozen/garbled, never self-heals).
  `video_pump` calls `note_frame_index()` every frame (fires throttled RFI on a forward gap) plus a
  throttled `request_keyframe()` backstop when `frames_dropped()` climbs.
- **Freeze-until-reanchor, adapted for NDL**: a forward gap or `frames_dropped` climb arms a
  `holding` flag; while held, frames are withheld from `ndl.play` (panel keeps its last picture, not
  a corrupted one) until one arrives with `FLAG_SOF` (IDR) or `USER_FLAG_RECOVERY_ANCHOR`. Upstream
  `punktfunk_core::reanchor::ReanchorGate` assumes a decode/present split, but `NDL_DirectVideoPlay`
  decodes+presents in one opaque call (no v3 API exists), so this client reimplements just the
  skip-until-reanchor subset. Gap vs. the shared gate: intra-refresh `USER_FLAG_RECOVERY_POINT` waves
  can't be consumed (holding skips the intervening frames they need); such hosts heal via the
  keyframe backstop instead, which is slower.
- HDR mastering metadata can change mid-session — `video_pump` drains `next_hdr_meta` every frame and
  forwards it, rather than fetching once at connect.
- `disconnect_quit()` fires only on a deliberate user stop (long-press Back) — host tears the virtual
  display down immediately. Other exits leave the connection to close normally.

## High-bitrate / high-resolution decode (current state)

Choppiness above ~80 Mbps and above 1080p was chased hard. **Shipped, confirmed fixes:**
- **Frame pacer**: `video_pump` feeds NDL immediately every frame (a prior fixed-schedule sleep
  starved the head start high-bitrate frames need — NDL couples decode+present with no decode-ahead).
- **`punktfunk-core` bumped** to negotiate `VIDEO_CAP_STREAMED_AU` and **ChaCha20-Poly1305**
  (`VIDEO_CAP_CHACHA20`, advertised unconditionally in `session.rs::connect` — the one cipher this
  client speaks, no toggle). ChaCha over AES-GCM matters: the CX is Cortex-A9/ARMv7-A with no ARM
  crypto extensions, so AES-GCM ran as O(bytes) software AES on one core, scaling with bitrate;
  ChaCha's add/rotate/xor core stays fast without crypto instructions. Requires a v0.17.2+ host.
- **Vendor decode-thread renice** (`spawn_vendor_decode_thread_renicer`): NDL's internal GStreamer
  pad-task threads (`<element>:src`) run inside our process at nice 0, invisible to core's hot-thread
  registry — real contention on this 3-core SoC. Renicing them to -10 fixed NDL choppiness above
  1080p outright. Matched by `:src` suffix.

**Still open** (parked, not client-side leads): Starfish stays choppier above 1080p despite the
renice generalization (its pipeline may not use `:src` naming, or priority isn't its bottleneck); and
a prior data point suggests the *host* itself may not always produce the requested fps at high res —
a host-side capture/encode throughput question, not this client. See
[memory: resolution choppiness] — client-side fixes ruled out, current lead is host-side encode.

## Runtime/deploy gotchas (LG CX)

- Apps install to `/media/developer/apps/usr/palm/applications/<appid>/`, which is also `$HOME` —
  the app's own writable directory (`store::app_dir`), where the versioned log file
  (`punktfunk-webos-<version>.log`) and `connect.conf` live.
- `luna-send` **needs `ssh -tt`** (a real PTY) or its output is silently swallowed even on success.
- **Black screen despite correct decode**: launch through the real app lifecycle
  (`luna-send .../launch`, jailed uid under SAM), never a raw SSH exec — NDL's punch-through plane
  only composites for the real SAM-managed foreground app. (Install/launch luna-send invocations are
  in `Taskfile.yml`'s `deploy` task.)
- No env vars through a SAM launch, but `params` given to `applicationManager/launch` DOES reach a
  native app — SAM JSON-encodes it as `argv[1]` on initial launch (confirmed against the webOS OSE
  native-app docs, contradicting an earlier assumption here). `src/logger.rs` reads a `telemetry`/
  `telemetry_level` key from it this way (see `task deploy TELEMETRY=...`); the older
  `$HOME/connect.conf` dev-override file predates this finding and could likely become a launch
  param too, but hasn't been converted.
- SDL2/Wayland may report `refresh_rate=0` in some launch contexts; clamp to a real default.

## Confirmed platform limitations (don't try to "fix" these again)

- **Frame rate only paces the stream — it can't set the panel's refresh rate.** `webosbrew/SDL-webOS`
  exposes only a read-only `SDL_webOSGetRefreshRate`; there is no set-side webOS API from a homebrew
  app. aurora-tv/moonlight-tv/Kodi all only read it. Panel scan-out is fixed at the system level.
- **Magic Remote Back requires `SDL_WEBOS_ACCESS_POLICY_KEYS_BACK`** set before window creation (else
  the launcher intercepts it). With it, Back arrives as `keycode = 2097155` (`WEBOS_BACK_KEYCODE` in
  `ui.rs`), caught via raw `i32` compare → `MenuEvent::Back`.
- **A hidden/unmapped window gets no pointer input.** Don't `.hide()` the stream-time window (that
  broke Magic Remote pointer → host-mouse forwarding). Keep it mapped and cleared fully transparent
  (`RGBA(0,0,0,0)`) each frame so the NDL plane shows through — same as aurora-tv.
- **Two independent cursors.** webOS draws its own local cursor tracking the remote; the host draws a
  second at our forwarded position, over the network — never in sync. Fixed by hiding the local
  cursor during a stream (`show_cursor(false)`). `mouse.rs` also scales motion by `SENSITIVITY` 0.55.
- **Color buttons (Red/Green/Yellow/Blue) require raw scancode polling**, not the safe SDL2 event
  API. The fork adds `SDL_SCANCODE_WEBOS_RED=486`.. which vanilla SDL2 and rust-sdl2's safe
  `Scancode`/`Keycode` enums don't cover (`from_i32(486)` → `None`). `ui::webos_red_button_down()`
  reads the raw keyboard-state array directly (level read; caller edge-detects).
- **Don't re-add an in-stream diagnostics overlay via window show/hide.** Toggling
  `window.show()`/`.hide()` while NDL's plane is compositing silently killed the process (uncatchable
  native Wayland crash). If wanted, treat as new work and test window-visibility changes in total
  isolation first.

## Known gaps / not yet done

- **HDR** is fully wired but not yet visually confirmed on a real HDR-negotiated session.
- **Gamepad in-stream passthrough** (`gamepad.rs`) is wired but not verified with a real controller
  mid-stream (menu nav via gamepad works).
- **Magic Remote pointer during a stream** is not forwarded to the host (menu-only). To add: the
  absolute-pointer wire shape is `flags = (width << 16) | height`.
