# ADR: Experimental frame pacing (PTS smoothing)

## Context

webOS exposes no vsync hook; video is presented via `NDL_DirectVideoPlay(buffer, size, pts)` — opaque decode+present with no observable scanout. Bursty network delivery clusters frames with near-identical timestamps, causing judder even at the correct average rate. This ports aurora-tv/SS4S's **PTS smoothing** (relabeling burst frames onto an evenly spaced grid) as a default-off feature without touching the stable path.

## Decision

Port SS4S's smoothing as an **experimental, default-off**, live-toggleable module.

- **`PtsPacer`** — advances an "ideal" PTS by a fixed interval, clamped to `PACE_MAX_DRIFT_FRAMES = 0.5` around the real reference; `PACE_MIN_STEP_NS = 1_000_000` keeps values strictly increasing after NDL's ms-truncation.
- **`HostPtsAnchor`** — maps host capture-clock PTS onto NDL's player clock (first-frame anchor) to isolate delivery jitter from the drift reference.
- **`reconciled_pace_interval_ns(stream_hz)`** — reads panel Hz via `SDL_webOSGetRefreshRate` (clamped 20–240); anchors to panel cadence within **±2 Hz** of stream fps, otherwise keeps stream interval. Software quantization only — **not** real vsync.
- **Base reference**: Starfish → `frame_pts_ns`; NDL → `ndl.elapsed_ns()` via `HostPtsAnchor`.
- **Live toggle**: `StreamStats::pacing_enabled: AtomicBool` Arc-shared between threads; re-anchors on off→on edge; edge-detected in `main.rs` with log, overlay redraw, and toast. Stats overlay shows `Pace ±X.X ms`.
- **Settings**: `Screen::Experimental` (`src/app/experimental.rs`) hosts the toggle via an **Experimental** action row in Settings.
- **Input**: Toggle uses **Blue (489)** (`WEBOS_BLUE_SCANCODE`). Red (486) is OS-intercepted; Green (487) and Yellow (488) are confirmed working.

## Consequences

**Positive:** stable path untouched; fully isolated in `src/pacing.rs`; A/B-able live without reconnect; reuses SS4S constants.

**Risks:** NDL effect unproven (may be a no-op if PTS is used only for A/V sync); delta ≤ ½ frame interval truncated to ms, so differences only appear under bursts; not real vsync.

## On-device evaluation

1. `task deploy TV_HOST=... TELEMETRY=auto`
2. Green → show overlay; Blue → toggle pacing; confirm log line and toast.
3. `Pace ~0.0` = nothing to smooth; nonzero/drifting = pacer active (no improvement → NDL ignores PTS for scheduling).
4. If Blue doesn't register, poll scancodes and fix `WEBOS_BLUE_SCANCODE`.

## Key files

| File | Purpose |
|------|---------|
| `src/pacing.rs` | Pacer, anchor, panel reconciliation, FFI, constants |
| `src/session.rs` | `pace_base_ns`, `pacing_enabled`, `video_pump` loop |
| `src/ndl.rs` | `NDL_DirectVideoPlay`, `elapsed_ns`, `render_buffer_length` |
| `src/ui/input.rs` | Color-button scancodes, `webos_scancode_down` |
| `src/ui/notification.rs` | Toggle toast |
| `src/main.rs` | Blue-button toggle, overlay/toast rendering |
| `src/app/experimental.rs` et al. | Experimental submenu wiring |
| `docs/NOTES.md` | On-device findings |
