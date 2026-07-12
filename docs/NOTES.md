# Architecture notes and hard-won gotchas

This document captures the non-obvious decisions, platform limitations, and debugging trails
from building this client, so they don't have to be rediscovered. Developed and verified against
a real **LG CX, webOS 5.6**, using root SSH access for logs/testing.

## Toolchain (reproducible via `task toolchain` — see `Taskfile.yml`)

- Cross target: `armv7-unknown-linux-gnueabi` (Rust tier-2) + `webosbrew/native-toolchain`'s
  `arm-webos-linux-gnueabi-gcc` (buildroot, GCC 12.2.0). Ships a native **macOS arm64** build too
  — no Docker/VM needed on Apple Silicon.
- `.cargo/config.toml` wires the linker/CC/pkg-config env vars to `scripts/cc-shim.sh`/
  `cxx-shim.sh`, which pass `--sysroot` explicitly — this toolchain's baked-in default sysroot
  path is stale post-relocate.
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

Deliberately flat SDL2 2D primitives (rects, rounded-rect via per-scanline circle math, `SDL2_ttf`
text) — no Skia/Vulkan available on webOS. Renders with LG's own on-device system font
(`/usr/share/fonts/LG_Smart_UI-Regular.ttf`) — **assume it only reliably covers ASCII**: an
earlier attempt at a "⚙ Settings" row using the U+2699 gear glyph rendered as a broken box.
Anywhere an icon is needed, draw it as vector shapes instead (see `ui::draw_gear_icon`) rather
than relying on a font glyph.

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

**The Magic Remote's hardware Back button is intercepted by webOS's system launcher** before it
reaches a native app's event queue at all — in both menu screens and during an active stream, and
not consistently across firmware/models. This is a **known upstream issue**
(`mariotaku/moonlight-tv#179`, "Stream overlay not working") — moonlight-tv's own documented
workaround is the same one this client uses: rebind in-app "Back"/"disconnect" to a **color
button** instead of relying on the hardware Back key (moonlight-tv's wiki: *"Long press BACK or
press exit button to open in-stream overlay"*). No `appinfo.json` flag, luna-service call, or
SDL-webOS API exists to claim the hardware Back button for a native app (`disableBackHistoryAPI`
is web-app/DOM-only, architecturally inapplicable to a native SDL2 binary).

This client's fix: the Red color button (scancode 486 on the SDL-webOS fork — see below) is the
reliable Back substitute. A short press applies `Back` to whatever menu screen is current; a long
press (1.5s, mirroring moonlight-tv's own long-press convention) during an active stream
disconnects back to the menu. The keyboard Escape/Backspace/AC_Back bindings are kept as
best-effort secondary handlers (they do sometimes arrive), just never relied on as the only path.

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

## Removed: the in-stream diagnostics overlay (2026-07-12)

An earlier pass added a Magic Remote Green-button toggle for an in-stream log/stats overlay
(`session::SharedStats`, `logbuf::Logger`'s ring buffer, `main.rs`'s `render_stream_overlay`). It
was removed entirely — not just disabled — after it crashed the app on the real CX the first time
it was exercised live: toggling `window.show()`/`window.hide()` on the normally-hidden SDL2 window
(hidden during streaming so NDL's punch-through video plane shows through unobstructed) while NDL's
hardware video plane was actively compositing killed the process silently (no panic message, no
`Result::Err` logged — it just disappeared from `ps aux`), almost certainly a native crash inside
the Wayland backend that Rust can't catch or convert into a recoverable error.

If an in-stream overlay is wanted again, treat it as a new feature rather than reviving this one,
and in particular:

- Test any window-visibility change in complete isolation first, logging immediately before and
  after each SDL call, so a crash pinpoints exactly which operation caused it.
- Confirm whether per-pixel alpha transparency on a freshly-shown window actually composites over
  NDL's video plane at all on this compositor — `SDL_SetWindowOpacity` (whole-window uniform
  opacity, a *different* mechanism) was already confirmed unsupported here ("That operation is not
  supported"), which doesn't answer the per-pixel-alpha question but suggests this compositor's
  window transparency handling in general hasn't been proven reliable.

Loss recovery (`note_frame_index`/`request_keyframe`) and continuous HDR metadata polling — added
in the same pass as the diagnostics overlay, but functionally unrelated to it — were kept; see
below.

## Cross-checked against the upstream embedding guide (2026-07-12)

Upstream punktfunk ships a C-ABI embedding guide (`docs/embedding-the-c-abi.md` in the
punktfunk repo) aimed at ports that link `punktfunk-core` directly rather than through this
Rust crate — but the underlying protocol/lifecycle contract is identical either way. Diffing
this client against its checklist (§15) turned up one real gap, now fixed, plus two smaller ones:

- **Loss recovery was completely missing** — the single most important item in the guide
  ("this is the part you must get right"). `video_pump` only ever read `frames_dropped()` for
  the stats overlay display; it never called `client.note_frame_index()` or
  `client.request_keyframe()`/`request_rfi()`. Under punktfunk's infinite-GOP stream (no
  periodic IDRs), unrecoverable loss produces reference-missing delta frames the decoder
  *silently conceals* — no decode error, just a frozen/garbled picture that would never
  self-heal without an explicit recovery signal. Fixed: `note_frame_index(frame.frame_index)`
  on every received frame (cheap, idempotent, fires a throttled RFI request internally on a
  forward gap), plus a throttled (`KEYFRAME_REQUEST_MIN_INTERVAL = 100ms`) `request_keyframe()`
  backstop when `frames_dropped()` climbs — the guide's own "complete, correct recovery policy
  on its own" combination.
- **HDR mastering metadata was fetched once, not continuously.** The original code called
  `next_hdr_meta` exactly once, synchronously, right after connect. The guide is explicit that a
  host can emit *updated* metadata over the life of a session (different content, different
  mastering values) and the client should "apply the latest." Fixed: moved into `video_pump`'s
  loop as a cheap non-blocking (`Duration::ZERO`) drain every frame, applying whatever arrives
  to NDL, instead of a one-shot fetch at connect time.
- **`disconnect_quit` was never called.** The guide distinguishes a deliberate user "stop" (host
  tears down the virtual display immediately) from a network drop/backgrounding (a plain `close`
  — via `Drop` — lets the host linger for a reconnect). This client's long-press-Back-to-disconnect
  is unambiguously the former, so it now calls `connected.client.disconnect_quit()` right before
  breaking out of the stream loop. Every other exit path (host ended the session, app quit)
  deliberately leaves this alone.

Not gaps, already correct: identity persistence + PIN pairing + fingerprint pinning; connecting
off the UI thread and building the decoder from the *resolved* codec/color (`client.mode()`/
`client.codec`), never the request; one thread per plane (video on its own thread, audio pumped
from the main thread only — see above); `flags = (w<<16)|h`-style pointer semantics don't apply
here since Magic Remote pointer input is only used for this client's own pre-stream menu, never
forwarded to the host as `MOUSE_MOVE_ABS`/touch during an active stream (a deliberate scope
choice, not an oversight — this client doesn't offer host-side mouse/touch control at all yet).

## Known gaps / not yet done

- **Game library poster art**: the library screen currently shows a plain text list. Showing
  cover art would need an image-texture loading/caching pipeline (fetch via the host's
  `/api/v1/library/art/...` proxy, decode, cache as SDL textures) — not yet built.
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
