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
  Discovery/art background result, or the raw scancode Red/color-button edge, all of which now set a
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

## UI rendering performance, round 3: cached-layer composition + grid scrolling (2026-07-22)

Triggered by an on-device report that the menu UI was sluggish and the game grid couldn't scroll
at all (rows past the second were laid out below the panel with no scroll state anywhere).

**The question "is tiny-skia a dead end, should the backend be replaced?" was evaluated — answer:
no.** The measured cost was never software rasterization per se; it was *re-pushing every card
through tiny-skia's general pipeline on every dirty frame* (~28 pipeline blits/frame — at the
measured per-call/area cost that alone reproduces the observed ~170-190ms Home frame). The
alternatives all lose: SDL2-accelerated per-widget textures was the original backend (replaced for
AA quality, and it had its own per-widget cost); real LVGL was already evaluated and rejected
(see below); GLES2-direct is a rewrite of every drawing primitive for a 4-screen menu. Raw
row-`memcpy` composition, in contrast, is measured fast on this SoC (`blit_layer`/`dim` history
above).

Architecture now (second iteration, same day — the first moved composition to raw CPU blits at
~43ms/frame, then was superseded by moving composition to the GPU outright once animations were
wanted): **hybrid rasterize-CPU / composite-GPU**. tiny-skia still rasterizes every widget (the
AA/soft-shadow look is untouched), but into standalone cached *tiles* (`ui::render_card_tile`/
`render_focused_row_tile`/`render_focus_ring_tile`/text tiles + the sidebar strip + the modal),
each owning a GPU texture in `compositor.rs`. `App::prepare_tiles` re-rasterizes only stale tiles
(art arrival = that one card); `App::draw_list` emits per-frame texture-copy commands the
`opengles2` SDL renderer executes. Position, scroll, the focus pop's scale, and fades are dst-rect/
alpha parameters — a pure animation/nav frame costs ~zero CPU, which is what makes the 60fps
animations (eased scroll, card focus pop, modal fade/slide — `App::tick_animations`) viable on
this SoC. Content frames measured on-device: ~38-41ms when a cover art arrives (rasterize + upload
one tile), one-time ~460ms full-library tile build. Notes: tiles are premultiplied-alpha —
`Compositor::upload` un-premultiplies on upload since SDL's `BlendMode::Blend` expects straight
alpha; `SDL_RENDER_SCALE_QUALITY=1` (linear) is required or the pop shimmers; the sdl2 crate's
`unsafe_textures` feature lets the texture cache live in a struct. The redraw-on-change loop
gained one nuance: `tick_animations` keeps frames flowing while anything animates, and only
`content_dirty` frames re-rasterize.

If profiling is ever needed again: the render-cost log line in `run_ui_flow` ("render: Xms",
currently a TEMP diagnostic, content frames only) is the ground truth on-device; re-add it rather
than guessing.

## High-bitrate video decode choppiness (2026-07-21)

Symptom: decode visibly lags the stream and framerate gets choppy above ~80 Mbps, unusable
above ~100 Mbps — despite the CX's hardware decoder being confirmed capable of 150+ Mbps
butter-smooth by aurora-tv, over the *same host*, via its GameStream-compatibility protocol
path. Root-caused by reading aurora-tv's source/history and the `punktfunk-core`/
`punktfunk-host` source directly, not just this client.

Two real, fixed contributors:
- **Frame pacer regression (85b6ef5)**: `video_pump` slept to a fixed `next_present_at`
  schedule before every `ndl.play()`, withholding an already-available frame from the decoder
  until a scheduled instant. `NDL_DirectVideoPlay` couples decode+present with no decode-ahead
  of its own, so this shrank exactly the head start large (high-bitrate) frames need. Removed —
  feed NDL immediately on every frame, same as before that commit.
- **`punktfunk-core` was pinned to v0.16.0, missing `VIDEO_CAP_STREAMED_AU`** (added
  post-v0.16.0, default-on for the host's Linux direct-NVENC encoder as of v0.17.0): lets the
  host stream a multi-slice frame's tail overlapped with packetize/FEC/pacing instead of
  waiting for the whole frame to finish encoding — upstream's own gate numbers show p99
  encode-to-send latency 8527→5363µs on large frames. Bumped the pin to v0.17.0;
  `NativeClient`'s public API is unchanged, so no client code changes were needed — this crate
  already sent the capability bit unconditionally, it was just a no-op against an older
  host/core. Requires the host machine's own `punktfunk-host` rebuilt to v0.17.0+ too, or it
  ignores the bit.

Confirmed on-device: 100 Mbps went from choppy/unusable to usable and mostly stable.

**Known remaining gap, not yet fixed — the likely dominant remaining cost**: punktfunk's
*native* protocol (what this client speaks) negotiates AES-128-GCM on every video datagram
(`punktfunk-host/src/native.rs`), decrypted client-side per packet. The CX's SoC is
Cortex-A9/ARMv7-A — the ARM Crypto Extensions (dedicated AES instructions) only exist on
ARMv8+, so this runs as constant-time software AES-GCM on a single core, an O(bytes) cost that
scales with bitrate. aurora-tv's GameStream-compatibility path is explicitly plaintext
(`punktfunk-host/src/gamestream/video.rs`: "AES-GCM video encryption is negotiated off for
now") — zero decrypt cost, part of why it sustains 150+ Mbps.

The fix is **not** disabling encryption: swap to **ChaCha20-Poly1305** (RFC 8439, same
security tier as AES-GCM, a standard TLS 1.3 AEAD). Its core is 32-bit add/rotate/xor — no
S-box lookups, no GHASH carry-less multiply — so it stays fast in pure software on a CPU with
no crypto instructions at all (why Google/BoringSSL default to it on ARM devices without an
AES-NI equivalent). `punktfunk-core/src/crypto.rs` already builds on RustCrypto's `aead` traits
around `Aes128Gcm`; `chacha20poly1305` is the same crate family with the same trait shape, so
the in-crate change is close to a type swap. The real work is the wire-visible part: needs a
capability/version negotiation (every client and host must agree, so not a silent swap), and
the key grows from 16 to 32 bytes. This is a `punktfunk-core`/`punktfunk-host` change — affects
every client, not just this one.

**Status (2026-07-23): shipped and confirmed working.** `punktfunk-core` v0.17.2 negotiates
ChaCha20-Poly1305 (`VIDEO_CAP_CHACHA20`, advertised unconditionally in `session.rs::connect` —
no client-side setting/toggle, this is the one cipher this client speaks). Confirmed on-device:
sustains meaningfully higher bitrate than AES-GCM did before it. The *grant* still isn't
client-observable (`NativeClient` doesn't expose `Welcome::cipher`) — only the host's own log
shows which cipher a given session actually resolved.

## Resolution-dependent choppiness above 1080p (2026-07-22 – 2026-07-23)

Symptom: both NDL and Starfish are butter-smooth at a *captured* (host-side) resolution of
1080p, but choppy above it (1440p, 4K) — independent of bitrate or requested fps. aurora-tv is
smooth above 1080p on the exact same TV/host, but over its GameStream-compatibility path, which
is unencrypted and hits a different host code path — not a clean apples-to-apples reference for
this symptom.

**Tried, confirmed no measurable effect:**
- **Starfish: reordering `SDL_webOSSetExportedWindow` to after `StarfishMediaAPIs_load()`**,
  matching ss4s's `StarfishResourcePostLoad` timing — no change on its own, but kept: this is now
  simply how `starfish.rs::load` binds the punch-through window (see the ordering there), not a
  toggle.
- **PTS smoothing/pacing** ported from aurora-tv's ss4s fork — anchor the host's PTS to a local
  clock once, then either walk an idealized fixed-fps-interval grid, or just follow the host's
  PTS deltas with a monotonic floor. The grid variant looked smoother on NDL but added real input
  latency (holding frames back for a nominal cadence) and that improvement was never confirmed
  reproducible; the non-grid variant removed the latency cost but fixed nothing. Neither helped
  Starfish. Not committed — the actual reference `ss4s` (checked out locally) turns out not to
  do any PTS smoothing at all (`SS4S_NDL_webOS5_GetPts` is plain wall-clock-since-load, same as
  this client's own `ndl.rs`), so this was a custom design, not a direct port.
- **Starfish `pauseAtDecodeTime: false`** — no change. Retested on a clean, correctly-isolated
  build (an earlier attempt's negative result was suspect due to a mismatched build) — same
  negative result confirmed. Not committed.

**Fixed (2026-07-23) for NDL — large, confirmed improvement. Starfish remains choppier despite an
attempted generalization; parked, not pursued further for now.** Renicing the NDL/Starfish vendor
`.so`'s internal decode-pipeline threads to -10 fixed NDL outright. These are `GStreamer`-element
pad-task threads (`"<element>:<pad>"`, truncated to the kernel's 15-char `comm` limit) spawned
*inside our own process* by the vendor library, invisible to punktfunk-core's hot-thread registry
(that only covers threads this crate and punktfunk-core spawn themselves) and confirmed via live
`/proc/<pid>/task` sampling to sit at default nice 0 despite doing real decode work — a real
contention cost on this SoC's **3 CPU cores** (`nproc`-confirmed on-device).
`session.rs::spawn_vendor_decode_thread_renicer` matches by the `:src` pad-name suffix rather than
the two exact names observed under NDL (`lxvideodec1:src`/`video-src:src`), on the theory that
Starfish's own internal pipeline uses the same `GStreamer` pad-task convention with different
element names. **Retested after generalizing: no change for Starfish** — either its pipeline
doesn't follow the `:src` naming convention, or thread priority isn't its bottleneck at all. Not
investigated further this pass (would need live `/proc` sampling during an active Starfish
session to find its actual thread names, or a different hypothesis entirely) — kept as a known,
open gap rather than guessed at blindly. The suffix-based match and background-thread renicer
are kept as shipped, confirmed-correct behavior for NDL regardless.

**Still open**: a prior data point (2560x1440@120fps/150Mbps, NDL) showed the host's own frame
*arrival* rate cycling ~76-120fps with zero client-side drops/gaps ever flagged — suggesting the
host itself wasn't always producing 120fps at that resolution, a separate, possibly-compounding
host-side capture/encode throughput question, not yet re-examined after the thread-priority fix.

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

Text renders in punktfunk's brand font, **Geist** (2026-07-23; previously LG's on-device
`LG_Smart_UI-Regular.ttf`) — the exact OTFs every other punktfunk client bundles, copied verbatim
from `pf-console-ui/assets/fonts/` into `assets/fonts/` (OFL license alongside) and embedded via
`include_bytes!` (`ui::load_font`, weights Regular/Medium/SemiBold/Bold). The sidebar header and
the splash both come from the brand's ACTUAL logo artwork (`assets/logo/punktfunk-logo-dark.svg`,
rasterized at display size — see `assets/logo/NOTICE.md`), not a hand-drawn approximation.
**Assume text fonts only reliably cover Latin**: an earlier attempt at a "⚙ Settings" row using
the U+2699 gear glyph in the LG font rendered as a broken box. All 10 icons this UI uses (tv, lock, add, close,
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

**Magic Remote Back button requires `SDL_WEBOS_ACCESS_POLICY_KEYS_BACK`.** Set before window
creation — without it webOS's system launcher intercepts the key before SDL sees it. With the
hint active, Back arrives as `keycode = 2097155` (`WEBOS_BACK_KEYCODE` in `ui.rs`; SDL's webOS
extension, not a named sdl2 `Keycode` variant), and `menu_event_for_key` catches it via a raw
`i32` comparison and maps it to `MenuEvent::Back` alongside `Escape`/`AcBack`.

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

## UI overhaul: module split, list modals, About screen (2026-07-24)

Driven by a concrete complaint: there was nowhere to *put* new UI. Anything acting on a single
host (speed test, edit, remove) had no entry point, and adding a screen meant touching eight
places across two 2,400-line files.

**Both files are now split** — `ui.rs` -> `src/ui/` (14 modules along its existing section
banners) and `app.rs` -> `src/app/` (one module per screen). Two things made this safe rather
than a rewrite: `ui/mod.rs` **glob-re-exports** every submodule, so all ~700 `crate::ui::X` call
sites kept resolving with zero edits; and Rust lets one inherent `impl App` be spread across
modules of the same crate, so each screen module owns its own `impl App { .. }` block. The cost
is that `App`'s fields had to widen from private to `pub(crate)` — still crate-private, since
`mod app` itself is private to the binary.

**`ui::ListModal`** is the actual extension point. Every modal used to carry its own card
geometry, shell renderer, `ModalShellKey` variant and focused-widget rendering arm. A screen
built on `ListModal` supplies only a `Vec<FocusRow>` and what Confirm on row *i* does; geometry,
the unfocused shell, and the focused-row tile (the *same* `render_focus_row_tile` Settings
already used, so the focus-pop animation comes free) are shared. `RowKind::Action` is the new
control-less row kind this needs. `Screen::Settings` deliberately did **not** move onto it — its
rows carry live dropdown/slider/switch controls and an overlay, which is exactly the complexity
`ListModal` leaves out.

Note what the split does *not* fix: the `Screen` enum still has **eight** dispatch sites (`back`,
two mouse handlers, four in `prepare_tiles`, one in `draw_list`, one in `main.rs`). The compiler
finds them all at once via non-exhaustive-match errors, so it's mechanical rather than risky, but
a new screen is still eight small edits plus its module.

**Per-host actions are a visible ⋯ button, not a gesture.** The first cut used a 500ms hold on
OK — it worked, but nothing on screen said the actions existed, which is the whole problem with a
hidden gesture. `ui::sidebar_menu_button_rect` puts a ⋯ target on *every* host row (drawn muted in
the cached sidebar layer, so it costs nothing per frame). Reaching it without a pointer needed one
navigation change: Right on a host row now lands on that row's ⋯ button, and Right again continues
to the grid, so the button is on the natural path rather than behind a special key. Removing the
gesture deleted the whole hold-vs-tap interception in `run_ui_flow` — six event arms across
keyboard/controller/mouse plus a per-tick deadline poll, all of which existed only to tell a hold
from a tap — and with it `App::host_row_focused`/`focus_host_row_at`. Clicks now dispatch on
press again instead of waiting for release. The ⋯ hit target is 52px against a 26px glyph: this is
a 10-foot UI driven by a wobbly pointer.

**The version marker moved out of the sidebar** into `Screen::About`, matching every other
punktfunk client (they show it on their About/licences screen, not in nav chrome). It also rides
along as the Settings row's value, so it's visible without opening the screen.

**The About screen must never lay out its whole document.** `THIRD-PARTY-NOTICES.txt` is ~10,000
lines / 558 KB. Two rules, both load-bearing on this SoC: scrolling is by *source line* and
`draw_about_body` stops the moment it runs past the viewport bottom (cost is bounded by viewport
height, not document length); and the body draws through `draw_text_uncached`, **not**
`TextCache`. The cache is deliberately unbounded — its docs argue entry count is bounded by the
app's own content — and a licence wall breaks that assumption completely: scrolling the whole
document through a cached `draw_text` would leave ~10,000 rasterized `Pixmap`s resident with no
eviction path.

## Network speed test (2026-07-24)

`punktfunk-core` already had everything: `NativeClient::request_probe(target_kbps, duration_ms)`
plus `probe_result() -> ProbeOutcome`, both present in the pinned v0.19.1. No core bump was
needed. The probe is a decode-less connect (720p, no NDL/Starfish load, no pump thread — same
shape as `session::request_access`), then a burst polled to completion.

Two things are deliberately **different from every other punktfunk client**, both specific to
this hardware:

- **The burst is 1 Gbps / 3 s, not 3 Gbps / 5 s.** The filler is decrypted on the same 3-core
  Cortex-A9 that runs the UI thread, so an unbounded firehose starves the app for the whole
  measurement — and no webOS decode path here will ever consume anything near 3 Gbps, so the
  extra headroom buys no information. 1 Gbps is still far above what this client can use, which
  is all the burst needs to find the ceiling.
- **The probe connect must advertise `VIDEO_CAP_CHACHA20`,** exactly as a real session does.
  `punktfunk-core` increments its `bytes_received`/`packets_received` counters *after* AEAD
  decrypt (`session.rs`, past the `Err(_) => continue // undecryptable noise` guard), so the
  whole measurement is bounded by decrypt throughput. A probe that negotiated AES-GCM would
  report a ceiling this CPU cannot reach with the cipher an actual stream uses — a number no
  session could ever deliver.

Which means the figure here is **end-to-end deliverable goodput**, not link speed: whichever of
the network and this TV's own decrypt throughput gives out first. That is the more useful number
for choosing a bitrate (it's what a session could actually carry), but it is not what a desktop
client's speed test reports, and the UI says so in as many words.

**Measured on-device 2026-07-24, and the burst was retuned because of it.** Against a
0.19.2 host over Wi-Fi, a 1 Gbps request was honoured exactly — 374,996,992 bytes in
3,000 ms — while the CX received 87 MB of it: **~80 % loss**. Worse, in half of four
attempts the host's end-of-burst `ProbeResult` never arrived at all: it travels over the
QUIC control stream, through the very path the burst had just saturated, so it was being
lost or starved out. Overshooting capacity is how a probe finds a ceiling; overshooting it
fourfold mostly measures the access point's drop policy and costs you the result message.

The target was retuned to 400 Mbps then (and to 320 later, below), chosen against what
the answer can be *used* for rather than against the link: the bitrate slider caps at
200 Mbps and the recommendation is 70 % of measured, so **anything above ~285 Mbps
already yields an identical clamped recommendation**. Measuring past that point buys
nothing and actively degrades the measurement. Delivered figures at 1 Gbps were 232 and
248 Mbps, both of which recommend the same clamped value as anything higher would.

Corollary for anyone tempted to raise it again: the useful ceiling for this probe is set
by `ui::BITRATE_MAX_KBPS`, not by the link.

### G5 deep-dive (2026-07-24): the ceiling is the TV's own radio, and cold flows black-hole

Chasing the ~250 Mbps "ceiling" on an LG G5 with a headless probe run over Dev-Mode SSH
(`src/bin/pfprobe.rs` — the app's measurement as a CLI, runnable without the TV UI; feed
it the app dir's `client-cert.pem`/`client-key.pem` and the pinned fingerprint as hex)
produced three findings, each confirmed against kernel counters (`/proc/net/dev`,
`/proc/net/snmp`) sampled around the burst:

- **~245-253 Mbps is the real airlink ceiling of the G5's Wi-Fi, full stop.** A warm-path
  sweep at 260/280/320/400 Mbps offered delivered a flat ~242-247 Mbps every time, with
  loss exactly tracking the overshoot (24 % at 260 → 51 % at 400) and essentially none of
  it client-side (`RcvbufErrors` +43 over a whole burst). A raw Python UDP flood from a
  wired Mac — no punktfunk anywhere — delivered the same 253 Mbps. The G5's Wi-Fi module
  (MediaTek, `043e:310d`) hangs off an internal **USB 2.0 High-Speed bus** (480 Mbps,
  `/sys/class/net/wlan0/device/speed`), so ~300-350 Mbps is its physical best case
  regardless of PHY rate; ~250 through air is about right. The same host measured from
  the wired Mac took the full 400 Mbps burst at 0.00 % loss, so host and LAN are clean.
  **No client code can raise this number** — the probe was already reporting the truth.
- **A NEW host→client UDP flow is sometimes black-holed for ~10-29 s** (AP/driver flow
  setup — it swallowed even the session's own 20 Mbps video while QUIC control on the
  same radio chatted at ~1 ms RTT; a concurrent 10 Hz ping never stuttered, so it is
  per-flow, not the link; longer holes follow longer idle). A burst fired into that
  window measures the black hole, not the link — reproduced at every offered rate from
  100 Mbps up. Once the path has carried the flow it stays warm across sessions for at
  least minutes. Hence `run_speed_probe` now **waits for the first completed video frame
  (cap 35 s) before requesting the burst** — the plane has proven itself live, and every
  measurement lands on a warm path. Worth knowing for real sessions too: a cold stream start can sit in
  the same hole (the jump-to-live flush cleans up the pile-up when it breaks).
- **The target is now 320 Mbps** (was 400): with the true ceiling ~245, 400 offered only
  raises the shed overshoot (51 % vs 38 % packet loss) and with it the odds the
  end-of-burst report is starved out, for zero extra information — 320 still detects any
  ceiling that would change the clamped recommendation (>~285).

Also observed while there: `net.core.rmem_max` on webOS is 512 KB (core's 32 MB request
is granted ~416 KB-1 MB; unraisable without root), `pin_thread_user_interactive` is a
no-op on plain Linux (only Apple/Android get the pump boost), and 2 of the 4
Cortex-A78-class cores were offline at idle — none of which turned out to matter for the
probe ceiling, but all worth re-checking if streaming-session loss (not probe loss) ever
becomes the complaint: the box-lifetime `Udp: RcvbufErrors` counter equalled `InErrors`
exactly (17 k since boot), i.e. every UDP drop this TV has ever recorded was a
socket-buffer overflow during real sessions.

Two further things that on-device run exposed:

- **`throughput_kbps` is 0 until the host's report lands.** It divides by
  `ProbeState::host_duration_ms`, which only arrives with the end-of-burst report — so the
  "Mbps so far" progress line as first written could never display a figure. `recv_bytes`
  *is* live throughout (core computes it as `rx_now - base`), so the progress line reports
  bytes received instead.
- **A missing report no longer throws the run away.** Because `recv_bytes` is live, a
  timed-out probe that still received a real amount of filler is salvaged: divide it by the
  burst window we requested (the host honours that exactly when it does report — a 3,000 ms
  request came back as `elapsed_ms=3000`). `SpeedProbeResult::confirmed` carries the
  distinction to the UI, which labels an unconfirmed figure as a floor and omits the loss
  percentage, since loss genuinely cannot be computed without the host's sent-packet count.

One implementation note: `ConfirmButton` borrows its label, and every other confirm-button pair in
the app is a compile-time constant. The speed test's primary button is built from the measurement
("Use 84 Mbps"), so the label is owned by the caller and passed in by reference —
`Box::leak`-ing it to satisfy `'static` would leak on **every dirty frame** while the modal is
open, not once per test.

## The software keyboard was never broken — it was never asked for (2026-07-24)

webOS ships a real on-screen keyboard and `webosbrew/SDL-webOS` wires it up: the bundled
`libSDL2-2.0.so.0.3000.12` contains `SDL_waylandwebos_osk.c`, `show_input_panel`/
`hide_input_panel`, a `zwp_text_input_v3` implementation and `text_input_commit_string`
(confirmed by `strings`/`nm` on the sysroot copy — worth re-running before assuming any other SDL
capability is absent here). `SDL_HasScreenKeyboardSupport`/`SDL_StartTextInput` are all exported.

Nothing in this client ever called `SDL_StartTextInput`, so the panel simply never appeared and
the Add-host screen's only input was the remote's number pad. `run_ui_flow` now starts text input
on entering a text-editing screen and stops it on the way out (edge-triggered — `StartTextInput`
re-shows the panel on this backend, so calling it every tick would fight the user dismissing it),
and passes the field's rect via `SDL_SetTextInputRect` so webOS doesn't cover what's being
edited. Committed text arrives as `Event::TextInput` (whole strings, not synthetic key events),
so it needs its own handler alongside the number-pad path. `has_screen_keyboard_support()` is
logged at UI start-up.

## Host reachability, audio channels, library order (2026-07-24)

- **Presence dots.** mDNS only ever *adds* (`discovery::browse` handles `ServiceResolved`
  and logs everything else, `ServiceRemoved` included) and a saved host isn't discovered at
  all, so a host powered off for a week looked exactly like one that was up — you found out
  by pressing OK and waiting for a timeout into the Wake prompt. `app::reach` sweeps every
  entry every 30 s on one background thread via `NativeClient::probe` (a bounded,
  trust-agnostic, mDNS-independent handshake — no identity, no session), 2 s per host, and
  badges the result onto the row's icon. **`None` — never probed — draws no dot at all**: an
  unknown state must not look like a confident "offline".
- **Audio was pinned to stereo in the request only.** `audio.rs` has always handled up to 8
  channels (`MAX_CHANNELS`, `layout_for`), and `AudioPlayer::new` is built from the host's
  *resolved* count — the literal `2` was in `session::connect`'s request argument. Now a
  Settings dropdown (Stereo / 5.1 / 7.1). The host still clamps to what it can capture, so
  the request is a preference, never a promise.
- **Library order.** The host returns its own scan order. On a TV navigated one card at a
  time with a d-pad that's the difference between finding a game and sweeping the whole
  library, so `drain_games` sorts case-insensitively by title. "Desktop" stays card 0.
- **CI now lints.** `.github/workflows/build.yml` only ever built and packaged, which is how
  an `items_after_statements` violation sat on main unnoticed despite the crate carrying a
  curated `[lints.clippy]` set. `task native:lint` runs before the build.

**Still not enforced: `cargo fmt`.** The tree was already non-clean before this pass (85
`Diff in` hunks at HEAD, 132 after), so a formatting gate would need a separate
normalisation commit first — deliberately not folded in here, since it would bury every
functional change under unrelated churn.

## The on-screen keyboard covers a centred modal (2026-07-25)

Reported on-device: opening "Edit address" raised webOS's OSK **over** the card it was
meant to be typing into.

`SDL_SetTextInputRect` is set to the field (the correct contract — it tells the platform
which region to keep clear) and webOS ignores it. There is also no way to ask how tall the
panel is: `SDL_webOS.h` publishes only cursor / panel-resolution / refresh-rate /
exported-window calls, `SDL_IsScreenKeyboardShown` is a bare bool, and the fork's internal
`input_panel_rect` isn't reachable from the public API.

Since the height can't be queried but *is* stable — LG's keyboard occupies roughly the
bottom half regardless of layout — `ui::KEYBOARD_PANEL_FRAC` encodes it as a constant, and
the address card is centred in the band above it **only while the panel is actually up**
(`SDL_IsScreenKeyboardShown`, polled per tick). With the keyboard down the card sits
exactly where every other modal sits.

Two things worth keeping straight if this is revisited:
- The trigger is *panel visibility*, not "we called `SDL_StartTextInput`". webOS lets the
  keyboard be dismissed while the field keeps focus, and the card should drop back down
  when that happens — those two states are not the same.
- The lifted position is **centred in the remaining band**, not pinned to the top edge. The
  goal is clearing the keyboard, not jamming the card against the top of the panel; a
  `KEYBOARD_MIN_TOP` floor keeps a tall card from being pushed off-screen anyway.

An earlier cut anchored the card high unconditionally to avoid the card moving when the
panel opens. That was the wrong trade: it made the screen look broken in the common case
(keyboard down) to avoid one transition in the rarer one.

**A latent sizing bug fell out of the same investigation.** `add_host_card_rect` measured
its height against the *Add host* subtitle — a fixed one-liner — but `Screen::EditHost`
reuses that card with a subtitle carrying the host's name, which wraps to two lines for a
long one. The card was then a line too short and the input field hung out through the
bottom. Both screens now derive the subtitle from `App::address_subtitle` and size from it,
so the string that is drawn is by construction the string that was measured.

## Pairing card restructure + modal copy pass (2026-07-24)

**The pairing card presented two alternatives as if they were a sequence.** The PIN row came
first and held focus; "Request access" sat at the bottom of the card looking like the step
*after* filling the PIN in. Only the subtitle said otherwise, and nobody reads a subtitle to
work out a screen's structure.

It now reads primary-first: the accent-filled **Request access** button (the path that always
works — the PIN additionally needs the host's pairing page open and armed), an explicit
`ui::draw_or_divider` rule, then the PIN row as the labelled alternative. Focus defaults to
the button. Exclusivity is now structural rather than something to be read and remembered.

Two supporting primitives, both in `ui::modal`:
- `draw_or_divider` — a rule broken by a centred word. Without it the two blocks read as
  steps no matter what the copy says.
- `draw_primary_button` — accent-filled. Everything else in this UI uses the surface-card
  treatment where *focus alone* supplies emphasis, which is right when the options are
  peers; a genuinely preferred option has to read as preferred **before** it is focused.
  The focused tile uses the same fill, so focusing it doesn't demote it.

Navigation follows the new geometry: the button's Down/Right drops to the PIN row, and Left
off the first digit climbs back to the button. Right no longer tabs off the last digit onto
the button — the button is above the digits now, so that gesture stopped corresponding to
anything. Up/Down keep spinning digit values (the odometer is still the only way in on a
remote with no number pad).

All four y-positions come from one `pairing_layout` — the renderer, the mouse hit-test,
`prepare_tiles` and `draw_list` previously each rebuilt their own slice of that arithmetic.

**Copy pass across the modals.** The inconsistencies only showed up once every user-facing
string was listed together:
- The Wake modal was titled "Host unreachable" even when it was *offering an action*, which
  made an actionable card read as an error report. `wake_title` now asks ("Wake this host?"),
  reports progress ("Waking host…"), or states the fact ("Host unreachable") — the last only
  when no MAC is on record and there is genuinely nothing to offer.
- Its toggle read "Always send automatically" — send *what*? Now "Wake automatically in
  future". Its button read "Wake"; now "Wake host", matching the menu row that opens it.
- Host-name references were quoted in some places and bare in others, and split between
  "your host list" and "this TV". Standardised on bare names and "this TV".
- The host menu's "Connect (pairs first)" parenthetical moved into the value column, which
  is where every other `RowKind::Action` row puts its hint.

## Presentation research: the target device is no longer a CX (2026-07-25)

Everything above this line was written against an **LG CX, webOS 5.6, Cortex-A9, 3 cores**.
The device now under test reports something quite different, and several load-bearing
assumptions in these notes do not carry over:

| | CX (these notes) | G5 (`OLED65G58LW`, measured 2026-07-25) |
|---|---|---|
| webOS | 5.6 | **10.3.1** (Rockhopper, kernel 5.4.268) |
| CPU | Cortex-A9, ARMv7-A | **ARMv8-A `aarch64` kernel**, `Features: aes pmull sha1 sha2 crc32 asimddp` |
| Cores | 3 | **2** (`/sys/devices/system/cpu/online` = `0-1`), max 1.4 GHz |
| Userland | 32-bit | 32-bit (`webos_imagename: lib32-starfish-global-secured`) |
| RAM | — | 2.5 GB (1.9 GB visible) |

Consequences worth being explicit about:

- **The AES argument for ChaCha20 no longer holds *for the CPU*, but the conclusion still
  does.** This chip has ARMv8 Crypto Extensions. However the userland is 32-bit, so the
  binary stays `armv7-unknown-linux-gnueabi`, and RustCrypto's `aes` crate has an ARMv8
  intrinsics path for `aarch64` **only** — 32-bit ARM falls back to software regardless.
  ChaCha20 remains the right cipher here; the *reason* is now the 32-bit userland, not the
  absence of the instructions.
- **`-C target-cpu=cortex-a9` is wrong for this hardware.** It targets an in-order ARMv7
  core with VFP3; this is an ARMv8.2-class core (`asimddp`, `lrcpc`, `fphp`). Retuning
  would likely buy real codegen improvement — but it would also **drop CX support**, since
  a binary built for a newer baseline won't run on an A9. That is a product decision, not a
  technical one, and is deliberately left alone here.
- **Two cores, not three.** The vendor-decode-thread renicer (`:src` pad tasks) matters
  *more* here, not less: there is one fewer core to absorb contention.

### The two backends are front-ends to the same pipeline

`libgstlxvideodec.so` / `libgstlxvideosink.so` / `libgstdecproxy.so` are the platform's real
decode path, and both NDL and Starfish drive it underneath — which is why the `:src` pad-task
renicer works at all. "Replacing" them means driving GStreamer directly, which would also
mean reimplementing resource acquisition (`decproxy` picks the decoder "by resource
permissions") and the punch-through window binding. **There is no `/dev/dri`**, so there is
no DRM render node to import DMA-BUFs into either: presentation *has* to go through the
platform sink. The realistic ceiling on "alternatives" is therefore low, and the value is in
using the existing APIs more completely.

### Two NDL functions that were never bound

`NDL_directmedia_v2.h` declares, and the on-device `libNDL_directmedia.so.1` exports, two
functions this client never called:

- **`NDL_DirectVideoGetRenderBufferLength(int*)`** — now wired (heartbeat log + stats
  overlay). This is the signal that was missing: `NDL_DirectVideoPlay` decodes *and*
  presents in one opaque call, so "the decoder is behind" and "frames are arriving late"
  were indistinguishable — a slow `play()` could mean either. A rising backlog says the
  decoder; flat-near-zero while the picture stutters says upstream of it. That is exactly
  the open question left in the resolution-choppiness entry above ("host's frame arrival
  rate cycling 76-120 fps with zero client-side drops ever flagged").
- **`NDL_DirectVideoSetFrameDropThreshold(int)`** — bound, but **not called by default**.
  The units are undocumented (the header declares it and stops), and guessing a pacing
  value for this decoder is precisely what the entries above warn against. It reads an
  optional `$HOME/ndl-drop-threshold.conf` instead, so it can be swept on-device without a
  rebuild.

### AV1 is reachable — but only through Starfish

- `libgstlxvideodec.so` advertises `video/x-av1` (plus VP9, VP8, AVS2, and H.266/VVC): the
  **hardware decoder supports AV1**.
- `libplayerAPIs.so.1` (Starfish) contains `video/x-av1`.
- `libNDL_directmedia_impl.so.1` mentions only `H264`, `H265`, `VP90` — **no AV1**, despite
  `NDL_VIDEO_TYPE_AV1 = 4` existing in the v2 header.

So AV1 must not simply be added to the advertised codec set: on the NDL backend the host
could then pick a codec NDL won't decode. It would have to be advertised **conditionally on
the Starfish backend being selected**, and verified on-device before being trusted.

Why it matters here specifically: the G5 deep-dive established that the hard ceiling is the
TV's own Wi-Fi radio (~245 Mbps), which no client-side work can raise. A codec that needs
materially less bitrate for the same quality is therefore the largest remaining quality
lever on this device. It depends on the *host* being able to encode AV1, which is a
separate question about that machine's GPU.

## Opus offload to NDL + runtime device capability (2026-07-25)

**Audio can now be decoded by the TV instead of by us**, when the device will take it.
`NDL_DIRECTMEDIA_DATA_INFO_T`'s audio union has always had an Opus arm; this client zeroed
it (tag 0 = no audio) and ran `opus::MSDecoder` + an SDL2 `AudioQueue` instead. On a 2-core
1.4 GHz G5 that software decode is a more meaningful share of the budget than it was on the
CX's three cores.

Three constraints shape the implementation, and all three are load-bearing:

1. **Stereo only, by construction.** `NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T` carries a channel
   count and a sample rate and *nothing else* — no multistream mapping. punktfunk's 5.1/7.1
   are Opus **multistream** (`punktfunk_core::audio::layout_for` gives a stream/coupled/
   mapping triple per layout), which that struct cannot describe. Handing NDL `channels: 6`
   would have it decode plain 6-channel Opus and produce noise, not surround. Anything above
   stereo therefore stays on the software path — `session::ndl_audio_config` is the gate.
2. **Device support is probed, never assumed.** The header declaring the Opus arm says
   nothing about whether a given model's `libNDL_directmedia_impl` implements it, and this
   binary ships to TVs neither developer owns. `NdlVideo::load` therefore *tries* the
   audio-enabled load and, if it fails, immediately retries video-only;
   `audio_offloaded()` reports which one took. A model allow-list would be wrong the day a
   new TV ships — this is right on hardware that doesn't exist yet.
   **Known limit:** this detects a load that *fails*. A device that accepts the config and
   then plays nothing would be silent, which is why the chosen path is logged explicitly
   (`audio path: …`) rather than left to be inferred.
3. **`streamHeader` is passed null.** The header declares `const char *streamHeader` and
   documents nothing further. That is the most likely reason for a device to reject the
   config — which the fallback above exists to absorb.

Two things fall out of it beyond CPU. The offloaded path is drained by the **video pump
thread**, not the main loop: the main-thread rule exists only because `sdl2::audio::AudioQueue`
is `!Send`, and there is no `AudioQueue` on this path — so audio keeps flowing across a
main-loop stall instead of hitching with it. And feeding both planes off the same
`load_instant` clock lets NDL sync them itself, where the software path can only resnap its
own queue after the fact (`audio::MAX_QUEUED_LAG_MS`).

### `src/device.rs`

Runtime capability detection, because this client targets an open-ended set of TVs — a 2020
CX on webOS 5.6 and a 2025 G5 on webOS 10.3 are both current, and neither is the last.

**What it deliberately does not do is CPU codegen.** `-C target-cpu` is a compile-time flag,
so one `.ipk` cannot vary it per device; the baseline stays at the oldest supported model,
and that is the price of shipping a single binary. Behaviour is what varies.

**Preferred detection is by attempt, not by table** (the NDL audio probe above is the
model). The facts `DeviceInfo` gathers — core count, webOS major, model string — are for the
decisions that can't be probed cheaply, and for the startup log line that makes a bug report
from an unknown model actionable at all. The model string is diagnostics only; nothing
branches on it.

## Audio crackling on the software path (2026-07-25)

Reported after a 4K120 NDL session; the on-device log named the causes rather than
leaving them to be guessed at.

**Cause 1 — the latency bound was enforced with a sledgehammer.** `MAX_QUEUED_LAG_MS`
cleared the *entire* SDL queue whenever it grew past 100 ms, which is ~100 ms of silence,
per event. The log showed **five** `audio resnapped` events in a few minutes. A growing
queue does need something discarded (a realtime stream never drains a standing queue on
its own — the causes are a post-stall burst from punktfunk-core, or host/TV sample-clock
drift), but the whole queue is far more than necessary. A soft ceiling
(`SOFT_QUEUED_LAG_MS`, 60 ms) now drops **one 5 ms packet at a time** and lets the queue
walk back down; the hard clear stays only as a backstop for a burst arriving between two
pump ticks.

Subtlety worth keeping: a dropped packet is still **fed to the decoder** and its samples
thrown away. Opus is stateful, so skipping one outright leaves the decoder behind the
stream and corrupts what follows.

**Cause 2 — lost packets were never concealed.** punktfunk's audio datagrams carry no FEC,
so a lost 5 ms packet played out as a hard gap — a click. `punktfunk_core::audio` ships
`AudioGapTracker` for precisely this, and its own docs describe it as *"shared by every
platform decoder"* — this client was the one that never adopted it. `AudioPlayer::play`
now takes the packet's `seq`, asks the tracker how many are missing immediately before it,
and synthesizes that many libopus PLC frames first (`decode_float` with **empty input**
maps to a NULL data pointer, which is libopus's PLC entry point).

**Diagnostic added: underrun vs overrun.** These two were indistinguishable in the log and
have *opposite* remedies — an empty device queue means the feeding thread was too slow, a
full one means audio is arriving faster than realtime. `AudioEvent` now reports
`Underrun`/`Dropped`/`Resnapped`/`Queued` separately and each is logged distinctly. Note
the stats overlay (which was enabled during the reported session) renders and presents on
the **main thread** every 500 ms, on a 2-core device — a plausible underrun source, and
now a measurable one rather than a theory.

None of this affects the NDL Opus offload path, which does its own buffering and sync — but
the software path remains in use for 5.1/7.1 (NDL's Opus struct cannot describe multistream)
and on any device that rejects the offload, so it was worth fixing on its own terms.

## A large library froze the client (2026-07-25)

Reported against a **365-title** library. Two independent scaling faults, both from the
same assumption: that a library is small enough to materialize whole.

**Fault 1 — every card tile was rasterized and uploaded in one frame.** `prepare_tiles`
walked `0..count` and built every missing tile in a single pass the moment the grid went
dirty. At 1080p the grid is 5 columns of ~260x346 cards, so 365 titles meant ~366
tiny-skia rasterizations *and* ~366 GPU texture uploads back to back on the main thread —
and ~169 MB of `Pixmap` retained, plus the textures again, in a 32-bit process on a TV with
under 2 GB usable. (The "one-time ~460 ms full-library tile build" measured in the
composition entry above was a much smaller library; the cost is linear in title count.)

Now **windowed and budgeted**: tiles are built only for rows within `CARD_PREFETCH_ROWS` of
the viewport, at most `CARD_BUILD_BUDGET` per frame, and dropped beyond `CARD_KEEP_ROWS`
(deliberately larger than the prefetch window — the hysteresis is what stops a card
oscillating between built and evicted when the scroll sits on a boundary). This is safe
precisely because `Compositor::execute` already skips a tile with no texture, so a not-yet-
built card simply isn't drawn.

Two supporting pieces: `Compositor::drop_tile` actually destroys the SDL texture — with
`unsafe_textures` a `Texture` is not freed by dropping the map entry alone — and
`App::tiles_pending` keeps the redraw-on-change loop ticking while the window fills, since
it would otherwise go idle mid-build and leave the visible cards blank until the next input.

**Fault 2 — all cover art was fetched, decoded and retained up front.** `load_art_async`
walked the whole library on one thread: ~365 sequential mTLS round-trips on every launch
*and* every host switch, and at `MAX_ART_DIMENSION` roughly 200 MB of decoded pixmaps held
for the session. `art.rs` is now a request/response `ArtLoader`: the UI asks for the covers
its window is about to draw and forgets the ones it has scrolled away from.

**And art is now cached on disk** (`$HOME/art-cache/`), storing the *encoded* bytes exactly
as fetched — tens of KB each, versus hundreds of KB decoded. That keeps the cache small,
and it is what makes eviction affordable: scrolling back re-decodes from local disk instead
of re-fetching over mTLS. Written write-then-rename, and a cache entry that fails to decode
is deleted rather than left to poison that card for the life of the install.

Rough effect at 365 titles: retained card tiles fall from ~366 to ~40, and decoded covers
from 365 to the same window — tens of MB rather than hundreds.

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
