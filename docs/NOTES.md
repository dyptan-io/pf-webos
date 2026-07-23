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

**Fixed (2026-07-23), but NDL-only so far**: renicing the NDL/Starfish vendor `.so`'s internal
decode-pipeline threads to -10 — **large, confirmed improvement on NDL**. These are
`GStreamer`-element pad-task threads (`"<element>:<pad>"`, truncated to the kernel's 15-char
`comm` limit) spawned *inside our own process* by the vendor library, invisible to
punktfunk-core's hot-thread registry (that only covers threads this crate and punktfunk-core
spawn themselves) and confirmed via live `/proc/<pid>/task` sampling to sit at default nice 0
despite doing real decode work — a real contention cost on this SoC's **3 CPU cores**
(`nproc`-confirmed on-device). `session.rs::spawn_vendor_decode_thread_renicer` matches by the
`:src` pad-name suffix rather than the two exact names observed under NDL
(`lxvideodec1:src`/`video-src:src`), on the theory that Starfish's own internal pipeline uses
the same `GStreamer` pad-task convention with different element names — **not yet confirmed
whether the broader match actually catches Starfish's threads; still choppy there as of this
writing.** If Starfish remains unaffected after this generalization, the next step is live
`/proc` sampling during an active Starfish session specifically, to find its actual thread names
rather than assuming the `:src` suffix covers them.

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
