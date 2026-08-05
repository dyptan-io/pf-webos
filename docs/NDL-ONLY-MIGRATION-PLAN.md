# NDL-only migration — handoff plan

**Status:** not started. This is a handoff; no code has been written (any local
uncommitted edits from scoping were discarded). Everything below is to-do.

## Goal / decisions

Converge `pf-webos` on **NDL DirectMedia as the single video backend**, driven the way
aurora-tv/ss4s drives NDL, and make **audio decode through NDL the default** (ss4s-style),
replacing the broken hand-rolled "Opus audio offload". Two decisions fixed with the requester:

- **Video: behavioral parity, not a library link.** Keep the native `src/platform/webos/ndl.rs`
  binding — it is already a faithful port of ss4s's `modules/webos/ndl/webos5` module (same
  `NDL_Direct*` calls). Do **not** add the ss4s C library / a CMake submodule. Instead close the
  handful of bootstrap gaps vs ss4s (section D).
- **Audio: decode through NDL (ss4s-style) by default**, with automatic fallback to the existing
  software Opus→SDL path (`src/platform/webos/audio.rs`) when NDL can't take it (non-stereo, or
  the NDL audio-enabled load fails).

Removing Starfish also removes **AV1** — AV1 is only ever offered under Starfish (NDL cannot
decode AV1), so it is dead once Starfish is gone.

**Risk:** routing audio through NDL is the same class of path that previously froze webOS 10.3.
The fix (section D) is the kHz sample-rate correction + empty-frame decoder prime, matching ss4s;
it must be **device-verified**. Keep the software fallback and an escape-hatch override so there
is always a safe audio path.

Reference source for parity: clone `https://github.com/GuiDev1994/ss4s`, read
`modules/webos/ndl/webos5/{ndl_player,ndl_video,ndl_audio,opus_empty}.c`.

---

## A. Remove the Starfish backend

- **`src/core/model.rs`**: delete `enum VideoBackend` (`:64-70`) and the `video_backend` field
  (`:181-185`, default `:268`). Serde ignores unknown fields → an old `settings.json` with
  `"video_backend":"Starfish"` is silently dropped; no migration needed.
- **`src/platform/webos/starfish.rs`**: delete the file; drop `pub mod starfish;` in
  `src/platform/webos/mod.rs:12`.
- **`src/session/mod.rs`**: collapse `enum VideoPlayer` (`:46-49`) to a single NDL form (keep the
  `Arc<NdlVideo>` — the audio pump co-owns it, section C). Delete every `Self::Starfish` arm
  (`:58,73,86,93,102,118,124`). Rewrite `connect()`'s backend match (`:451-495`) to always
  `NdlVideo::load(...)` — no Starfish branch, no fallback. Remove `starfish_selected` (`:367`)
  and the `proven_unavailable()` use (`:371`).
- **`src/runtime/ui_flow.rs:48-52`**: drop the `starfish::proven_unavailable()` term.
- **Build**: in **`build.rs`** remove the `starfish_c_shim.cpp` compile + `-lplayerAPIs` link +
  `libplayerAPIs_C.so` emit (`:33-57`); delete `src/platform/webos/starfish_c_shim.cpp`. In
  **`Taskfile.yml` `stage-and-package`** drop `libplayerAPIs_C.so` from the ipk `lib/` list.
  Update the THIRD-PARTY-NOTICES entry (`build.rs:82-85`) to drop "Starfish (libplayerAPIs)".

## B. Remove Starfish-coupled UI + AV1

- **`src/ui/settings.rs`**:
  - Delete the **Video backend** row (`ROW_VIDEO_BACKEND` `:69`; value match `:248-260`; dropdown
    labels `:557`; index `:583-585`; apply `:620-630`; L/R cycle `:693-698`).
  - Delete the **Color range** row (`ROW_COLOR_RANGE` `:70-74,131-135,261-270,343-350`) — it is
    documented Starfish-only ("NDL has no equivalent field"), inert without Starfish.
  - Renumber remaining `ROW_*` constants; update `settings_visible_logical_rows()` (`:145-155`),
    the row-build removals, `dropdown_current_index`, and `apply_dropdown_choice`.
  - Drop AV1: `codec_options()` (`:477-495`) no longer pushes AV1; remove `AV1_CAPABLE` /
    `set_av1_capable()` (`:498-507`), `Av1 =>` in `codec_label()` (`:509-516`), and the
    stranded-AV1 fallback text (`:274-282`).
- **`src/app/state/settings.rs:81`** + **`src/app/view/settings.rs:11`**: remove the
  `ROW_VIDEO_BACKEND` handling and the Color-range layout comment.
- **`src/core/model.rs`**: remove `CodecPref::Av1` (`:80-89`).
- **`src/session/mod.rs`**: delete `ensure_ndl_can_decode` (`:291-296`), the AV1 clamp
  (`:359-383`), and `CodecPref::Av1` in the offered-codec match (`:385-393`).
- **`src/platform/webos/ndl.rs`**: remove `NdlCodec::Av1` (`:80,87-93,96-103`) and the AV1 line in
  the header comment.
- **`src/platform/webos/device.rs`**: remove `supports_av1()` (`:44-54`); drop its warm-probe
  spawn at **`src/app/mod.rs:600`**.
- **`src/services/store.rs`**: remove `dev_override_enable_av1()` (`:234-250`).

## C. NDL audio (ss4s-style) as the default; software as fallback

Keep all of `src/platform/webos/audio.rs` (`AudioPlayer`, `pump_audio_once`) — it becomes the
**fallback**, not the default.

- **`src/session/mod.rs` `ndl_audio_config()` (`:261-271`)**: remove the
  `dev_override_enable_ndl_audio_offload()` gate and the "known to freeze" warning. Return
  `Some(NdlAudioConfig{ channels: 2, sample_rate: 48.0 })` for stereo **by default**; return
  `None` for non-stereo (routes to software — NDL has no multistream mapping; ss4s's `opus_fix`
  re-encode for 5.1/7.1 is out of scope). Add an escape-hatch override in `store.rs`
  (e.g. `force-software-audio.conf`) that forces `None`, for on-device A/B.
- **Fallback**: `NdlVideo::load`'s audio-enabled load must fall through to a video-only load on
  failure (`audio_offloaded=false`) so `stream.rs` opens the SDL device and the software path
  runs. Verify this still holds.
- **`src/runtime/stream.rs:161-163`**: the `if connected.audio_offloaded { None } else {...}` gate
  is unchanged but now activates by default (NDL audio → no SDL device; software fallback → device
  opened).
- **`src/services/store.rs`**: remove `dev_override_enable_ndl_audio_offload()` (`:229`).
- Retained (now primary, not dead-unless-flagged): `session::ndl_audio_pump`,
  `Connected.audio_thread`/`audio_offloaded`, `VideoPlayer::audio_offloaded`/`ndl_audio_handle`;
  in `ndl.rs` `NdlAudioConfig`/`to_union`, `play_audio`, the audio-enabled load branch, and the
  `NDL_DirectAudioPlay` extern.

## D. NDL video/audio bootstrap parity vs ss4s `ndl/webos5` (implement — currently missing)

These are the deltas from the parity study; **none are in the tree** (scoping edits discarded).
All in `src/platform/webos/ndl.rs`:

1. **kHz sample rate.** `NdlAudioConfig.sample_rate` must be **kHz**: pass `48.0`, not `48000.0`
   (ss4s uses `sampleRate / 1000.0`). Passing Hz is the offload-freeze root cause. Set at the
   `ndl_audio_config()` call site (section C).
2. **Post-load empty-Opus prime.** Right after a successful audio-enabled `NDL_DirectMediaLoad`,
   feed one silent stereo Opus frame `[0xec, 0xff, 0xfe]` (ss4s `opus_empty_frame_211`) via
   `NDL_DirectAudioPlay` — "feed one empty frame to the decoder to ensure it is ready". Best-effort
   (non-fatal on error).
3. **Load-state callback.** Pass a real `extern "C"` callback to both `NDL_DirectMediaLoad` calls
   (currently null) that logs the states ss4s logs: `0x16` LOADCOMPLETED, `0x17` UNLOADCOMPLETED,
   `0x1a` PLAYING. `PLAYING` is the only signal the present pipeline actually started — useful for
   the framerate diagnosis.
4. **Do NOT add eager/startup NDL init.** ss4s inits NDL in its module `PostInit`, but doing that
   here (a `warm_init` before the SDL window/luna bring-up) breaks the first real load with
   `NDL_DirectMediaLoad failed: ret=-1 "player is not loaded"` — bring-up invalidates the early
   init and the idempotent guard then skips re-init. Keep `ensure_init` lazy inside `load()`.
   (See memory `project_ndl_eager_init_breaks_load`.)
5. Confirm the remaining ss4s bootstrap already matches: no `SetFrameDropThreshold`, no
   `NDL_DirectVideoSetArea`, combined audio+video in one `NDL_DirectMediaLoad`, audio PTS = ms
   since load. All already true — verification only.

## E. Docs / overlay strings

- **`src/ui/tiles.rs:238`** `STATS_OVERLAY_REF_LINE`: drop "Starfish".
- **`docs/NOTES.md`**: remove "Starfish (Opus multistream…)" (`:117-119`) and "AV1 support"
  (`:90-92`); note NDL is the sole backend and audio decodes through NDL by default.
- **`docs/Frame-Pacing.md:14`**: drop the Starfish clause.
- Remove Starfish/`pauseAtDecodeTime` doc-comments in `core/model.rs`, `device.rs`,
  `session/pacing.rs`, `runtime/ui_flow.rs`.

---

## Verification

1. **Build**: `task -s docker:check` → `task -s docker:lint` (clippy clean; many arms/consts are
   deleted) → `task -s docker:package` (ipk builds without `libplayerAPIs_C.so`).
2. **Settings UI**: no "Video backend" / "Color range" rows, no AV1 codec option; row
   focus/navigation and dropdowns line up after renumbering.
3. **On device** (`task deploy ... TELEMETRY=...`): start a stream, confirm log shows
   `NDL load state: LOADCOMPLETED` then `PLAYING` and audio path = "NDL hardware Opus decode";
   audio in sync, **no video freeze**. If it freezes, drop `force-software-audio.conf` and confirm
   software Opus→SDL still works. Check 1440p120/4K120 smoothness via the stats overlay
   (`Beat nz%`) — the audio-anchored pipeline is expected to help the present-phase issue in
   `docs/NDL-FRAMERATE-INVESTIGATION.md`.
4. **Old settings.json**: launch once with `"video_backend":"Starfish"` / `"codec":"Av1"` present;
   confirm it loads (values ignored, defaults applied), no crash.
