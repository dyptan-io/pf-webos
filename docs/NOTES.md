# Architecture & platform gotchas

Verified against LG CX (webOS 5.6) and G5 (webOS 10.3). Load-bearing decisions only.

## Toolchain

- Cross target `armv7-unknown-linux-gnueabi` (tier-2) + webosbrew toolchain. Linux-aarch64-only; CI native, dev Docker.
- `.cargo/config.toml` wires linker to `scripts/cc-shim.sh` (passes `--sysroot` explicitly).
- **Soft-float was single biggest perf fix** (~300ms → ~30ms per render). Non-`hf` target spec disables hardware FP codegen despite VFP3/NEON existing. Fix: `target-feature=+neon,+vfp3,-soft-float` + `target-cpu=cortex-a73` in `.cargo/config.toml`. Changes *codegen* only, not FFI ABI.
- **glibc shims required** (`src/glibc_compat_shim.c`): webOS glibc ~2.12 predates `getauxval`/`gettid`/`sendmmsg`. Linked via `cargo:rustc-link-arg`, **must land AFTER libstd** (single-pass linker drops `link-lib=static` too early).
- **SDL2 must be webosbrew fork** (release-2.30.12-webos.5, not generic SDL2). Only fork has Wayland shell-integration (`QT_WAYLAND_SHELL_INTEGRATION=webos`). On-device system copy is 2.0.10 (too old). Bundle own libSDL2 with `$ORIGIN/../lib` RPATH (set in `build.rs`).
- **cmake/opus**: `punktfunk-core`'s `quic` feature needs CMAKE_POLICY_VERSION_MINIMUM=3.5 (modern CMake refuses vendored libopus's old minimum).

## UI rendering

Hybrid software/GPU: `tiny_skia` rasterizes tiles, SDL2 composites. Redraw-on-change (no every-tick render). Key facts:

- **Never use `tiny_skia::Painter::draw_pixmap/fill_rect` for large areas** (~300ms full-screen). Use `pixmap.data_mut()` loop or `copy_from_slice`. Verify with on-device timing, never assume a call is cheap.
- Tiles use premultiplied-alpha; `Compositor::upload` un-premultiplies (SDL `BlendMode::Blend` expects straight alpha).
- `FilterQuality::Nearest` + `anti_alias=false` are cheaper scan-conversion paths.
- Fonts: Geist (OTF, embedded). Icons: Material Icons subset (~1.7 KB) — **subset, so new `ICON_*` codepoint needs font regenerated** (`assets/icons/NOTICE.md` has `pyftsubset` line + codepoint list). Assume Latin only.
- **Scroll fade needs viewport to cut mid-row, else invisible.** Unfocused rows draw no own background (`draw_selectable_fixed` fills only when focused), so a viewport ending on a row boundary has only card background in last pixels — fading `SIDEBAR_BG` into `SIDEBAR_BG` is a no-op; first attempt shipped rendering nothing. `ui::SETTINGS_PEEK` deliberately leaves partial row for `SCROLL_FADE_H` to dissolve.
- **Modal scrolling is pixel-based, offsets are row-based.** `scroll.offset` stays integral (focus logic + scrollbar defined in rows); `App::modal_scroll_px` is animated *rendered* crop, eased like Home grid. Pixels also let last row sit flush at list end — `offset * stride` overshoots by peek strip. Anything positioned against the list (focus tile, dropdown anchor) **must** derive from same pixel offset: focus tile is focused row re-rendered, so anchoring to quantized row shows that row twice during scroll. Can also hang past viewport mid-glide, hence clip in `draw_list`.

## Video decode (NDL DirectMedia)

- `libNDL_directmedia.so.1` is real device library; NDK sysroot ships link-time stub.
- PTS = milliseconds since `NDL_DirectMediaLoad`, not wall-clock.
- Audio decoded client-side via Opus (not routed through NDL).
- **Decouple decode dimensions from punch-through rect** — else 1080p stream on 4K panel punches only top-left quarter.
- **Loss recovery required** — no periodic IDRs in stream. `video_pump` calls `note_frame_index()` every frame (throttled RFI on gaps) + `request_keyframe()` backstop when `frames_dropped()` climbs.
- **Freeze-until-reanchor adapted for NDL**: NDL does decode+present in one opaque call (no split); client reimplements skip-until-reanchor subset. Forward gap arms `holding` flag; frames withheld until one arrives with `FLAG_SOF` (IDR) or recovery anchor.
- HDR mastering metadata can change mid-session — drain `next_hdr_meta` every frame.
- **`NDL_DirectVideoSetHDRInfo` forces panel into HDR mode on *any* call** (OLED65CX, webOS 5): ignores SDR `transfer`/`primaries` triplet, emits HDR infoframe regardless, so SDR/H.264 stream showed in HDR picture mode. Fix: `ndl.rs::set_color_info` no-ops when `meta` is `None` (SDR) — only genuine HDR mastering metadata reaches NDL. Cost: NDL can no longer fix a bitstream's missing VUI colour info; SDR relies on bitstream VUI. HDR also gated to HEVC end-to-end (`session::connect`: `apply_hdr = host_hdr && codec==H265`; explicit H.264 pick drops HDR caps + hides Settings toggle).

## DualSense feedback: Bluetooth service, not hidraw

Adaptive triggers work on **non-rooted** TV, but not through SDL. Verified end-to-end on G5 (webOS 10.3, dev-mode install, `DualSense` over Bluetooth): trigger resistance, section walls, lightbar colour all confirmed on real hardware.

- **No `/dev/hidraw*` in app jail** — not even with pad connected, no `hidraw`/`leds` class in `/sys` either. So SDL's HIDAPI PS5 driver + `SDL_GameControllerSendEffect` (both in bundled fork) never reach the pad. **Don't re-attempt via SDL, don't bump to SDL3** — no webOS SDL3 fork, and blocker is jail policy, not SDL's API.
- **What works instead**: `luna://com.webos.service.bluetooth2/hid/internal/sendData` writes arbitrary HID output report to pad. Permitted because `compat.api.json` places it in **`public`** API group, and `/usr/share/luna-service2/devmode_certificate.json` grants dev-mode app `["ares.webos.cli", "public"]`. Restricted `devices`/`bluetooth.manage` groups *not* needed.
- **Payload traps** (each cost hours): `reportData` must be int array **with no `reportId` key** — one extra property fails whole call with generic "does not match the expected schema" naming nothing. `setReport` never works (always error 4 "operation can not be performed at this time"); only `sendData` does. `getReport` *hangs* on a pad that doesn't answer, so callers need a deadline.
- **Report must be CRC-signed** exactly as kernel's `hid-playstation`: 78 bytes (`0x31`, seq<<4, `0x10` tag, 47-byte common block, 24 reserved, CRC32-LE), CRC over `0xA2` seed byte plus report body. **Wrong CRC is silently ignored by pad while service still answers `returnValue: true`** — most misleading failure mode here. Don't prepend `0xA2` to `reportData`; stack adds HIDP header itself.
- LG backported `hid-playstation` to kernel 5.4, so pad binds as three input devices (pad/motion/touchpad) sharing one `U: Uniq=` MAC — where `dualsense::find_address` reads it.
- **Rumble does not use this path**: pad's event node advertises `EV_FF`, group-writable by `compositor` (app's uid is in it), so rumble goes through SDL's evdev force feedback, working for any pad. Reports in `dualsense.rs` deliberately never set compatible-vibration valid-flag, so paths can't fight.
- `hid/internal/*` is undocumented vendor surface — feature-detected, failing soft, never assumed.
- **Feedback sends must be throttled, or video plane goes black.** Each send forks/execs `luna-send-pub`, copying page tables of a process holding SDL, decoder + buffers. A Steam/Gamescope host *animates* the `DualSense` lightbar, so unthrottled sender spawned dozens of processes/sec on 2-3 core TV — observed failure was **black panel with frame counter climbing, `dropped=0`, `backlog=0`**: decode thread is priority-boosted so it kept running while compositor never presented (audio underruns the other tell). `dualsense.rs` drops identical states, spaces rest by `MIN_SEND_INTERVAL`. Don't assume host feedback is human-paced; lightbar alone is not.

Host side: a game only emits trigger effects when it sees a `DualSense`, so pad kind in handshake decides whether this feature does anything. Settings' **Controller** row (`store::GamepadType`) defaults to `Automatic`, which **mirrors attached pad** (`gamepad::detect_type`) rather than sending wire `GamepadPref::Auto` — that wire value means "host decides", and host decides Xbox 360, which is why a `DualSense` first showed as Xbox pad with no effects. Resolution happens per session (`main::resolve_gamepad_type`), deliberately doesn't write back, so stored preference keeps meaning "match my pad". Host env `PUNKTFUNK_TEST_FEEDBACK` makes host send scripted lightbar/LED/trigger burst — use to test without a game.

## Known platform limitations (don't retry)

- **Frame rate paces the stream; can't set panel refresh rate.** `webosbrew/SDL-webOS` exposes read-only `SDL_webOSGetRefreshRate` only; no set-side webOS API. Used by `PtsPacer` (`session.rs::reconciled_pace_interval_ns`): when measured panel Hz is within ±2 Hz of stream fps, paced PTS grid anchors to panel's cadence instead of stream's (aurora-tv's `session_worker.c` trick). Still not real vsync — just PTS quantization to display's rate.
- **Magic Remote Back requires `SDL_WEBOS_ACCESS_POLICY_KEYS_BACK`** set before window creation. Arrives as `keycode = 2097155`. Same for Home (`SDL_WEBOS_ACCESS_POLICY_KEYS_HOME`) and Guide (`SDL_WEBOS_ACCESS_POLICY_KEYS_GUIDE`). Launcher ribbon overlay needs `SDL_WEBOS_ACCESS_POLICY_RIBBON=false` or it pops over the app.
- **A held Back arrives as EXIT key, not a long Back — don't time the hold yourself.** webOS does long-press detection: short Back tap delivered as Back key (`keycode 2097155`, no scancode), but *holding* Back fires webOS's own EXIT gesture, delivered as discrete `SDL_SCANCODE_WEBOS_EXIT = 505` press — held Back key itself never reaches app (confirmed on-device: long press logs no Back down/up at all). So "hold Back to open the dialog" can't work by timing Back events; instead poll `WEBOS_EXIT_SCANCODE` (edge-detected like colour buttons — 505 is outside rust-sdl2's `Scancode` enum so never surfaces in safe event API) and open disconnect/quit dialog on its rising edge. Short Back tap stays plain: forwarded to host as Esc (stream) or back-nav (menu). Exactly aurora-tv's split (`keyboard_webos.c`: EXIT→open overlay, BACK→VK_ESCAPE). Needs `KEYS_EXIT` (above) or gesture SIGTERMs instead of delivering 505.
- **Gamepad disconnect shortcuts must be holds, not presses** (`main::DisconnectChord`, 2 s). Guide, both shoulders, or Start+Back opens in-stream disconnect dialog — and every one of those buttons is also forwarded as real game input, which is the whole constraint: L1+R1 in particular is a common in-game binding, so press-to-fire would kill streams mid-play. Chord state tracked from transitions (SDL reports no held-state here), **cleared when it fires or pad unplugs** — an open dialog swallows controller events and an unplugged pad sends no releases, so without that the buttons stay logically down and the dialog reopens the moment it's dismissed.
- **Hidden window gets no pointer input.** Keep it mapped and fully transparent `RGBA(0,0,0,0)` each frame so NDL plane shows through (not `.hide()`).
- **Two independent cursors** — webOS draws local cursor; host draws second over network. Hide local cursor during stream (`show_cursor(false)`). `mouse.rs` scales motion by `SENSITIVITY` 0.55.
- **Color buttons (Red/Green/Yellow/Blue) require raw scancode polling.** `SDL_SCANCODE_WEBOS_RED=486`... not in vanilla SDL2. `ui::webos_red_button_down()` reads raw keyboard-state array directly.
- **Don't toggle window show/hide while NDL composites.** Silently kills process (uncatchable Wayland crash). Test visibility changes in isolation.

## Runtime gotchas (LG CX/G5)

- Apps install to `/media/developer/apps/usr/palm/applications/<appid>/` = `$HOME` (writable dir for logs, `connect.conf`).
- `luna-send` **needs `ssh -tt`** (real PTY) or output silently swallowed.
- **Black screen despite decode**: launch through real app lifecycle (`luna-send .../launch`, SAM jailed uid). NDL punch-through only composites for SAM-managed foreground app.
- No env vars in SAM launch, but `params` in `applicationManager/launch` reaches native app as argv[1] JSON (parsed by `src/logger.rs`).
- SDL2/Wayland may report `refresh_rate=0` — clamp to sensible default.

## ChaCha20 over AES-GCM

CX/G5 are 32-bit userland on ARMv8-A. RustCrypto's `aes` crate has ARMv8 intrinsics for `aarch64` only; 32-bit ARM falls back to software regardless. ChaCha20 (add/rotate/xor, no crypto instructions) stays fast. Advertise `VIDEO_CAP_CHACHA20` unconditionally in `session.rs::connect` — only cipher this client speaks.

## Large library handling

- **Tile windowing**: `prepare_tiles` builds tiles only for rows within `CARD_PREFETCH_ROWS` of viewport, at most `CARD_BUILD_BUDGET` per frame. Deliberately larger `CARD_KEEP_ROWS` (hysteresis stops oscillation).
- **Cover art**: `ArtLoader` request/response (UI asks for visible covers, forgets scrolled ones). Cached on disk as *encoded* bytes (`$HOME/art-cache/`, write-then-rename). Failed decodes deleted.
- Effect at 365 titles: retained tiles drop from ~366 to ~40; decoded covers from 365 to viewport window (~5 columns).

## Audio (software Opus path)

- **Soft ceiling (60 ms) drops one 5 ms packet at a time; hard clear (100 ms) backstops bursts.** Hard clear of entire queue = ~100 ms silence per event. Soft walks queue back down.
- **Lost packets concealed** with libopus PLC: ask `AudioGapTracker` how many precede current packet, synthesize that many PLC frames first (decode with empty input).
- Underrun vs overrun logged separately (`Underrun`/`Dropped`/`Resnapped`/`Queued`). Underrun sources: audio feed thread too slow, or main thread stats overlay renders every 500 ms on 2-core device.

## Opus offload to NDL (OFF BY DEFAULT)

**Freezes video on webOS 10.3.** Load succeeds, frames accepted at 120 fps with `frames_dropped=0`, but panel holds first frame forever. Opt-in via `$HOME/ndl-audio-offload.conf = 1`. No runtime test distinguishes a TV that offloads from one that dies silently. Keep opt-in until verified on new hardware.

## AV1 support (unreliable, opt-in)

Advertised by G5's silicon, unusable in practice. Silicon accepts AV1 load then silently presents nothing (or black screen, or process dies). NDL ignores it. Starfish sometimes times out. Gate: Starfish backend selected, decoder claims AV1, `store::dev_override_enable_av1() = 1` (file `$HOME/av1.conf = 1`). Settings row labels stranded choice `AV1 (unavailable)` rather than showing unused option. Keep wired for future testing; promote to real setting when a device plays it.

## ABR startup probe: 2 Gbps, upstream-hardcoded

**"Automatic" bitrate fires a 2 Gbps burst ~2 s into every session, and on Wi-Fi that can cost the session its video entirely** — not a slow start but a flow that never establishes. Measured on G5: a "successful" probe still reported `send_dropped=20211`, i.e. link hammered far past what it can carry (~245 Mbps airlink ceiling), and probes that get nothing back sit on core's 6 s timeout. Capped at 300 Mbps the same link reports `send_dropped=0-167` and stream starts are reliable.

Don't read a slow *start* as this bug — a host compositor coming up has its own startup time, and video legitimately arrives late on first connect of a session. Signal that matters: packet drops on the probe and video that never arrives at all.

This is `CAPACITY_PROBE_KBPS` in `punktfunk-core`'s `client/pump/data.rs` — a **hardcoded const with no cap knob**, still 2 Gbps as of core v0.22.2, directly at odds with this client's own speed test being deliberately capped at 320 Mbps for the same "unbounded firehose starves the app" reason (below).

**Fixed by capping the burst**: `main.rs` sets `PUNKTFUNK_ABR_PROBE_KBPS=300000` before anything spawns a thread (`setenv` isn't thread-safe, and core reads it while building its data-plane pump). 300 Mbps matches what this client already burst-tests its *own* speed probe at, still above the ~245 Mbps airlink ceiling this hardware reaches — measures the link without knocking it over. Knob is core-side; **core v0.22.3 is first release carrying it**, which is why the pin moved off v0.21.0. Against an older core the variable is simply ignored.

That bump also brought a `connect` signature change — a `name: Option<String>` (label the host's pending-approval list shows) between `launch` and `pin`. All four call sites pass `None`, preserving fingerprint-derived label; sending a real TV name is a separate user-visible change.

Blind alleys, so they aren't re-tried:

- `bitrate_kbps == 0` (Automatic) arms **both** the AIMD controller and this probe — client cannot separate them.
- `PUNKTFUNK_ABR_PROBE=0` disables the probe but leaves climb ceiling at negotiated start rate (~20 Mbps), which core's own comment calls a box "Automatic could NEVER climb out of".
- Running our own capped probe instead does **not** work: `request_probe` completes, but `abr.set_ceiling` is only called from core's own probe path (gated on its `capacity_probe_deadline`), so ceiling never moves. No public bitrate/ceiling setter on `NativeClient`.
- Pinning a fixed bitrate also disarms the probe, but costs mid-session adaptation entirely.

## Network speed test quirks

Burst is 320 Mbps / 3s (not 3 Gbps / 5s) — 3-core Cortex-A9 runs UI thread; unbounded firehose starves app. 320 detects any ceiling that changes clamped recommendation (>~285 Mbps). Probe must advertise `VIDEO_CAP_CHACHA20` like real session (core's `bytes_received` counter increments *after* AEAD decrypt). Measured on G5 Wi-Fi: ~245 Mbps airlink ceiling (MediaTek USB 2.0 Hi-Speed bus), nothing client code can raise. New flows sometimes black-hole ~10-29s (AP/driver setup); `run_speed_probe` waits for first completed video frame (cap 35s) before burst — plane is live and path is warm.

## Starfish (Opus multistream, 5.1/7.1 audio)

Header signatures from `mariotaku/ss4s`. Load succeeds via `StarfishMediaAPIs_load` callback; LOADCOMPLETED may never arrive. Try `Ss4s` payload first on all releases (webOS 5 shape works on modern webOS), `Modern` as never-successful fallback (added analysis inconclusive). No decode context handle; all NDL/Starfish calls serialized behind `NdlVideo::ffi` mutex (not thread-safe per header).
