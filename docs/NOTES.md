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
  `ui.rs`), caught via raw `i32` compare → `MenuEvent::Back`. Same story for a connected HID
  keyboard's Windows/Meta key (`SDL_WEBOS_ACCESS_POLICY_KEYS_HOME`/`_META`) and a gamepad's Guide
  button (`SDL_WEBOS_ACCESS_POLICY_KEYS_GUIDE`) — all three otherwise background the app into the
  launcher instead of reaching SDL2. The `KEYS_*` hints alone stop the *key event* from
  backgrounding the app but the launcher's card-switcher ribbon overlay is gated separately —
  also needs `SDL_WEBOS_ACCESS_POLICY_RIBBON=false` (paired the same way in aurora-tv's `app.c`)
  or it can still pop over the foregrounded app. (Hint names confirmed via `strings` on a real
  `webosbrew/SDL-webOS` `libSDL2-2.0.so.0`, not from any header available in this repo.)
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

### Codec picker (2026-07-25)

Settings now has a **Codec** row (Automatic / H.264 / HEVC / AV1). Three things worth
keeping straight:

- **It is a preference, not a demand.** The wire has both an advertised decode *set* and a
  soft `preferred_codec`; `resolve_codec` honours the preference only when the host's
  encoder can produce it, else falls back down its HEVC > AV1 > H.264 ladder. So "H.264"
  against an HEVC-only host still yields a session (HEVC), never a refusal. H.264 and HEVC
  were always in the advertised set — the picker is what finally makes the preference
  reachable. The `connected:` log line carries `offered=`/`preferred=` beside the resolved
  codec so a "why didn't I get X" report answers itself.
- **AV1 is triple-gated, and opt-in only**: the row offers it (and `session::connect`
  advertises it) only when (1) the user explicitly picked it, (2) the Starfish backend is
  selected — NDL's impl decodes no AV1 despite `NDL_VIDEO_TYPE_AV1` existing in the header
  (above) — and (3) `device::supports_av1()`: the platform decode element
  (`/usr/lib/gstreamer-1.0/libgstlxvideodec.so`, world-readable under the SAM jail) contains
  the `video/x-av1` caps string. That last check is the same inspection that established
  AV1 support in the first place, done at runtime; it fails closed, and it's what keeps the
  option off a CX-era panel whose silicon predates AV1. Never advertised un-picked, so the
  host's precedence ladder can't auto-select a path no one has verified on this panel.
- **Switching the backend away from Starfish clears a stranded AV1 preference** (in
  `apply_dropdown_choice`), and `session::connect` clamps it again anyway — the UI rule
  keeps state honest, the connect clamp covers a hand-edited/stale `settings.json`.

**On-device 2026-07-25, first AV1 attempt — two findings, one of them a freeze.**

- **NDL accepts an AV1 load and then silently presents nothing.** `NDL_DirectMediaLoad`
  with `kind = 4` returns **success** (`NDL_VIDEO_TYPE_AV1` is in the v2 header even though
  `libNDL_directmedia_impl.so.1` implements only H264/H265/VP9), every
  `NDL_DirectVideoPlay` returns success, the frame counter climbs, `frames_dropped` stays
  0, feed times look normal — and the panel holds **frame one** forever. The stats overlay
  says a healthy stream; the picture is frozen. This is the video-plane twin of the Opus
  offload's known limit ("a device that accepts the config and then plays nothing would be
  silent").
- **The gate has to live at the decoder, not only in the UI.** The picker offers AV1 only
  under the Starfish backend — but *selecting* Starfish does not mean Starfish **loads**,
  and the pre-existing `Starfish load failed → fall back to NDL` path then handed an
  already-negotiated AV1 stream to NDL. The codec is negotiated during the handshake,
  which happens **before** any decoder is opened, so by then it is too late to choose
  differently. `session::ensure_ndl_can_decode` now refuses that combination outright and
  returns to the menu with an actionable sentence.

### Starfish works on this G5 — it was AV1 that never loaded (corrected 2026-07-25)

**An earlier version of this section claimed Starfish was dead on webOS 10.3. That was
wrong, and the way it was wrong is worth keeping.** `StarfishMediaAPIs_load` did return
`true` with LOADCOMPLETED never arriving, every time, across many attempts — but *every
one of those attempts was an AV1 session*, because AV1 was the codec selected while the
new picker was being tested. Starfish had never once been tried with H.265 on this TV. It
loads H.265 fine, with the **untouched ss4s payload**.

The false conclusion then produced a false fix: a `Modern` payload shape was derived from
the platform binaries (see below) and tried first on webOS ≥ 10. It has never completed a
load anywhere, while the `Ss4s` shape it was meant to replace loads immediately — and
ordering it first cost a 4 s timeout on *every* Starfish session, during which the host is
already streaming, so frames pile up and the bitrate controller reads the resulting
flushes as congestion. `Ss4s` is now tried first on every release, with `Modern` kept only
as a never-yet-successful fallback.

Two lessons, both cheap to state and expensive to relearn: a failure mode reproduced "every
time" is only evidence about the configuration it was reproduced in — the codec was the
variable nobody was holding still; and derived-from-strings evidence (below) reads as far
more conclusive than it is.

Cause, as far as the evidence goes: the load payload is ss4s's **webOS 5** shape, and this
platform does not know parts of it.

The useful source turned out to be **`libNDL_directmedia_impl.so.1`** — NDL is a front-end
to the same playback framework (above), and NDL *works* on this TV, so the payload it
builds internally is a known-good webOS 10.3 reference. Its strings are:
`externalStreamingInfo`, `mediaTransportType`, `esInfo`, `pauseAtDecodeTime`,
`ptsToDecode`, `lowDelayMode`, `codec`, `windowId`, `adaptiveStreaming`, `appId`,
`format`, `maxWidth`/`maxHeight`/`maxFrameRate`, `option`.

What we send that appears in **neither** that library nor `libplayerAPIs.so.1`:
`transmission` / `contentsType` / `"WEBRTC"`, `bufferingCtrInfo`, `seperatedPTS`,
`provider` / `"Chrome"`, `streamQualityInfo*`, `audioSync`, `restartStreaming`. Prime
suspect is **`transmission.contentsType = "WEBRTC"`** — an unrecognized mode the pipeline
may sit in forever, which is exactly the observed symptom. (`BUFFERSTREAM` *is* in
`libplayerAPIs.so.1`, so `mediaTransportType` is fine. And note `pauseAtDecodeTime` IS
present in the NDL impl though absent from `libplayerAPIs.so.1` — proof that "absent from
one binary" was only ever suggestive, as suspected.)

`starfish.rs` therefore has **two payload shapes** (`PayloadShape::Ss4s` /
`::Modern`, the latter being the ss4s payload minus every key above), and
`StarfishVideo::load` **tries both** — webOS ≥ 10 gets `Modern` first, older gets `Ss4s`
first, and either falls through to the other. So the webOS major only orders the attempts;
it never decides the outcome, keeping the probe-by-attempt discipline while avoiding a
guaranteed timeout on a correctly-guessed device. The LOADCOMPLETED wait dropped 5s → 4s
since it is now potentially paid twice. The full payload is logged at INFO per attempt.

**That reasoning was tested and rejected**: `Modern` never loaded, `Ss4s` did. The string
evidence was real — those keys genuinely are absent from both libraries — but absent keys
were apparently ignored rather than fatal, so the inference "absent ⇒ rejected ⇒ hang" did
not hold. Kept here because the analysis is sound method against the wrong hypothesis, and
because the key inventory is useful if a future release really does change the schema.

(`libplayerAPIs_C.so` in the app's `lib/` is this repo's own shim from
`src/starfish_c_shim.cpp` — not a platform library, and not the problem.)

### AV1: advertised by the silicon, unusable in practice — now opt-in

The G5's `libgstlxvideodec.so` advertises `video/x-av1` and the host encodes AV1 happily
(it resolved `codec=4` when asked). Actually playing it failed **three different ways**
across a handful of attempts: the Starfish load timed out; or it loaded and presented a
**black screen with frames flowing**; or the process **died outright**. NDL cannot decode
AV1 at all and accepts it silently (above).

So `device::supports_av1()` — a scan for the decoder's caps string — answers "does the
silicon claim AV1", which is a much weaker statement than "can this TV play an AV1
stream", and is **not sufficient to offer the option**. AV1 now additionally requires
`store::dev_override_enable_av1()` (`$HOME/av1.conf` = `1`), so it is off by default in
both the picker and the negotiation, alongside three conditions that each rule out a
distinct way of handing a decoder something it can't present: the Starfish backend
selected, the decoder declaring AV1, and Starfish not having already failed to load this
run (`starfish::proven_unavailable()`, process-lifetime, never persisted). The Settings
row labels a persisted-but-unoffered choice `AV1 (unavailable)` rather than showing a
codec the session won't use.

The whole negotiation path stays wired, so AV1 is one file away from being testable on a
model that might do better. It should become a real setting when some TV plays it.

## Opus offload to NDL — OFF BY DEFAULT: it freezes video on webOS 10.3 (2026-07-25)

> **Read this before re-enabling anything below.** Handing NDL the Opus stream stops the
> **video** plane on an LG G5 (webOS 10.3): the audio-enabled `NDL_DirectMediaLoad`
> returns success, every `NDL_DirectVideoPlay` returns success, the pump feeds a steady
> 120 fps with `frames_dropped = 0` — and the panel holds the **first frame** forever.
> Reproduced three times; disabling the offload and changing nothing else fixes it
> immediately, confirmed on-device. The offload is now **opt-in** via
> `$HOME/ndl-audio-offload.conf` = `1` (`store::dev_override_enable_ndl_audio_offload`);
> the default is video-only NDL + software Opus.
>
> Three things worth carrying forward:
> - **The failure mode is worse than this feature's own docs anticipated.** They warned a
>   device might accept the config and play no *audio* (point 2 below). What actually
>   happens is that it also takes *video* down — and since `load` succeeds, the
>   probe-by-attempt detection has nothing to catch. There is currently **no runtime test**
>   that distinguishes a TV that offloads audio from one that dies quietly doing it.
> - **`backlog` is what identified it.** A healthy session shows the render buffer
>   occasionally at 1-2 frames and the host delivering ~76 fps; the broken one showed
>   `backlog` pinned at exactly 0 while accepting a rock-steady 120 fps — frames going in
>   and being discarded, not queued. Neither the frame counter, the drop counter, nor the
>   feed time distinguishes those two, which is why the stats overlay read perfectly
>   healthy over a frozen picture. This is the one signal that separates them; the
>   heartbeat logs it at INFO for that reason.
> - **It has never been confirmed working on any device**, including the CX it was written
>   against. Keep it opt-in until some model is verified end-to-end, then promote it.

Everything below documents the mechanism as built, and stays accurate for the opt-in path.

**Audio can be decoded by the TV instead of by us**, when the device will take it.
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

Two things fall out of it beyond CPU. The offloaded path is drained by a **dedicated audio
thread** (`session::ndl_audio_pump`; it first lived on the video pump thread — see the
2026-07-25 smoothness pass below for why that was a defect): the main-thread rule exists
only because `sdl2::audio::AudioQueue` is `!Send`, and there is no `AudioQueue` on this
path — so audio keeps flowing across a main-loop stall instead of hitching with it. And
feeding both planes off the same `load_instant` clock lets NDL sync them itself, where the
software path can only resnap its own queue after the fact (`audio::MAX_QUEUED_LAG_MS`).

Because two threads then drive one **singleton** C API (`NDL_DirectVideoPlay` /
`NDL_DirectAudioPlay` take no context handle, and nothing in the header claims
thread-safety), every `NDL_Direct*` call is serialized behind `NdlVideo::ffi`. That mutex
is *not* what fixed the freeze above — the freeze reproduced with it in place — but
"undocumented" has to be read as "not safe", and an uncontended lock is nothing against a
call that decodes and presents a frame.

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

## Streaming smoothness pass (2026-07-25)

Three changes out of a full audit of the latency path; each is small but load-bearing.

- **Offloaded audio has its own drain thread** (`session::ndl_audio_pump`). Its first home
  was the video pump loop, *after* a `next_frame` call that blocks up to 500 ms — so a
  video drought (host encoder stall, loss hold) chopped audio into ≤500 ms stalls with
  packets already waiting, and normal-flow packets drained in per-video-frame clumps that
  all took the same drain-time PTS. Core's `next_audio` docs ask for a dedicated thread
  outright ("packets arrive every 5 ms"; pull methods are one-thread-per-plane safe).
  Teardown is the subtle part: `NdlVideo::drop` unloads NDL process-globally, so the
  handle is now `Arc<NdlVideo>` shared between the two threads — the unload runs at last
  `Arc` drop, i.e. never before both threads have exited, with no join-ordering to get
  wrong.
- **The ABR decode signal now sees the render backlog.** `report_decode_us` fed core's
  controller the NDL `play()` duration, but NDL's play is decode-and-present in one
  opaque call — submission time, not decode state. A decoder quietly falling behind
  buffers internally while the feed stays fast, leaving `abr::DECODE_RISE_US` (built
  precisely for "decoder saturates before the link") blind here. The reported figure is
  now `feed_time + render_backlog × frame_period`, with the backlog polled every 250 ms
  (three samples per ABR report window — per-frame polling would be assuming an NDL call
  is cheap, the exact mistake warned about above) and cached between polls.
- **Renice outcomes are now one summarizing info line each** (hot threads and vendor
  decode threads: "N boosted, M failed"). The open worry was that `setpriority(-10)` needs
  `CAP_SYS_NICE`, which a plain Dev-Mode SAM jail plausibly withholds — in which case the
  renice wins recorded in these notes (measured on a *rooted* CX) would not transfer to a
  Dev-Mode install at all. **Measured 2026-07-25 on the Dev-Mode G5: they do apply** —
  `hot-thread renice: 2 boosted, 0 failed` and `vendor decode threads: 4 found, 4
  boosted`. So no unprivileged fallback is needed, and the notes' renice findings stand on
  a non-rooted install. Keep the lines: this is exactly the question a contention report
  from an unknown TV has to answer first.

**A "decoder loads after the handshake, so frames pile up" finding was withdrawn after
measuring a healthy session.** In the frozen sessions the ABR controller walked the
encoder from 20 Mbps to 6.86 Mbps before a frame was ever decoded — three jump-to-live
flushes, each read as severe congestion — which looked like a startup-ordering defect
worth fixing (the backend loads after the handshake, so `video_pump` can't drain until it
is open; 5.5 s in that session because of the Starfish timeout). **It does not reproduce
once the stream actually works**: a healthy 62 s session showed `dropped=0` throughout, no
jump-to-live, no ABR backoff at all, and the rate simply stayed at the negotiated 20 Mbps
with a 182 Mbps climb ceiling available. The pile-up was a symptom of the freeze (and of
Starfish's 4-5 s timeout), not an independent bug. What remains is one brief loss/recovery
~1 s in, caused by core's own 2 Gbps startup capacity probe saturating the link — the
price of measuring the ceiling, and cheap at one recovery.

Worth keeping as method: the frozen-session logs supported a confident, wrong diagnosis.
Only a comparison against a known-good session separated "caused by" from "co-occurring
with".

## Known gaps / not yet done

- **HDR** is fully wired but not yet visually confirmed on a real HDR-negotiated session.
- **Gamepad in-stream passthrough** (`gamepad.rs`) is wired but not verified with a real controller
  mid-stream (menu nav via gamepad works).
- **Magic Remote pointer during a stream** is not forwarded to the host (menu-only). To add: the
  absolute-pointer wire shape is `flags = (width << 16) | height`.
