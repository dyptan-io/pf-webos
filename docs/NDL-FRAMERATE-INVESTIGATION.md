# NDL 1440p120 / 4K120 "60-in-a-120-container" — investigation handoff

**Status:** open. Instrumentation landed; two candidate fixes agreed but **not yet built**.
One load-bearing assumption is **unverified** — read "Open question that gates the fix"
before building anything.

## Symptom

On high-mode NDL streams (1440p120, sometimes 4K120) the picture intermittently looks
like 60 fps inside a 120 container — visible judder, half-rate feel. Behaviour is
**miss-or-hit per session**, same scene / same game / same host:

- Pacer OFF: fresh app launch — 1st stream laggy, 2nd smooth, 3rd laggy (alternating).
- Pacer ON: 1st laggy, 2nd smooth, 3rd smooth (maybe luck), 4th laggy.
- **First stream after a fresh app launch is essentially always laggy.**
- **Bitrate does not matter** — seen buttery-smooth at 200 Mbps, juddery at 100 Mbps.
- 1080p120 is *always* smooth. Only 1440p120 / 4K120 (the pipeline margin) miss-or-hit.

## Root-cause model (current best theory)

Two distinct effects stacked:

1. **Cold-pipeline warmup** — the first NDL load after boot spins up the SoC / GStreamer
   decode graph under load, so the first session runs lean. Deterministic, separate from
   the coin-flip. → addressed by **boot warmup**.

2. **Load-time present-phase lock** — NDL does decode+present in one opaque call
   (`NDL_DirectVideoPlay`) and locks its **present clock to the panel vsync at load time**.
   That phase (first present relative to a vsync edge) is effectively random per
   `NDL_DirectMediaLoad`. At 1080p there's throughput headroom so phase never bites; at
   1440p120/4K120 the per-frame decode+present budget (~8.33 ms) is marginal, so a bad
   phase → every frame slips one vsync → clean 60. A **manual restart re-rolls** the phase
   (full unload+load), which is exactly what the user does today to get a smooth run.
   We cannot set this phase from our side — only re-roll it. → addressed by **auto-reroll**.

Why every stat is identical between a smooth and a juddery run: all our metrics are on the
**feed/submit side**; the defect is inside NDL's opaque present path, and NDL exposes **no
presented-frame or internal-drop API**. So submit-side numbers *cannot* differ. The only
NDL-side signal that couples to presentation is `render_buffer_length` (standing queue
depth) — see the instrumentation below.

Related prior findings (memory / docs): `project_cx_pixel_throughput_ceiling`
(present-side HDR/pixel ceiling), `feedback_webos_ndl_maxbitrate_unreliable`,
`docs/NOTES.md` line ~53 (no set-side panel refresh; `PtsPacer` only quantizes PTS).

## What was already tried, and what it did

| Change | Result |
| --- | --- |
| **PTS present lead** — stamp PTS ahead of NDL's clock to prime a standing cushion. Started as `$HOME/ndl-present-lead-ms.conf` dev-override, then **hardcoded** to `PRESENT_LEAD_FRAMES` (currently **3**, ≈25 ms at 120 Hz) in `session/mod.rs`. Applied as `pts_ns.saturating_add(present_lead_ns)` in `video_pump`. | Helped **partially** — smooth more often, not deterministic. A fixed lead primes a cushion at startup but a jitter spike can drain it, and with pacing OFF the base PTS is arrival-timed so the cushion has no stable grid to stand on. |
| **Frame pacing ON** (existing `PtsPacer`, Blue button / setting) | Did **not** resolve the coin-flip. Still miss-or-hit (see observations). Pacing smooths the *submission* grid but cannot set NDL's *present* phase. NOTE: the user's earlier lead tests were run with **pacing OFF** — a lead on an arrival-timed base is expected to be fragile; pacing-on tests since still coin-flip. |
| **Bigger lead (2 → 3 frames)** | Marginal; "a bit more often smoother." Not deterministic. |

Net: nothing so far makes it deterministic, consistent with an uncontrollable load-time
present phase that only a full reload re-rolls.

## Instrumentation added (kept — this is the diagnostic surface)

All in the stats overlay (green button); **all NDL sampling is gated on the overlay being
on-screen** (`stats.stats_overlay_shown`) so there is **zero extra cost in normal play**.
Fields on `StreamStats` (`session/mod.rs`), written by `video_pump`, read by
`runtime/stream.rs`:

- **`panel {Hz}`** (`panel_hz`, via `pacing::panel_refresh_hz` / `SDL_webOSGetRefreshRate`)
  — panel's *actual* refresh. Below stream rate = present-side (panel dropped Hz).
- **`buf N (pk M)`** (`render_backlog` + `render_backlog_peak`) — NDL undecoded/unpresented
  depth, sampled every 150 ms with a ~2 s decaying peak. Rising = decoder behind.
- **`Gap min/max ms`** (`frame_gap_min_us` / `frame_gap_max_us`) — wall-clock interval
  between consecutive fed frames. Feed/arrival cadence. Same for smooth vs juddery here
  (≈2/15 ms both) → arrival is **not** the cause.
- **`Beat nz% · beats/win`** (`ndl_buf_nonzero_pct` / `ndl_buf_beats`) — backlog sampled
  **once per fed frame** (not the 150 ms poll, which aliases an ~8 ms beat to 0). Fraction
  of frames with backlog>0, and transition count. **This is the standing-cushion signal.**
- Existing: `Video fps/target`, `Drop/FEC/hold`, `Feed ms (pk)`, `Mbps`, CPU/RAM,
  `Pace ±ms`.

**Decision tree** (what the overlay tells you):
- `fps ≈ 60` → arrival/network-bound.
- `fps ≈ 120` + `buf pk` rising → decode ceiling (`cx_pixel_throughput_ceiling`).
- `panel < stream` → panel dropped its own Hz (present-side).
- everything identical but miss-or-hit → **bootstrap present-phase** (this doc).

Key observed data point: on a **smooth** 4K120@200 Mbps run, `Beat nz` read **80–100%**
(NDL holding a cushion). High nz correlated with smooth. (Earlier notes briefly had this
inverted — corrected: **high nz = good**.)

## Open question that GATES the fix (verify first)

The auto-reroll plan assumes **low `nz` ↔ laggy run**. We have only confirmed
**high nz ↔ smooth**. We have **not** captured `nz` on a *laggy* run. If a laggy run also
shows high nz, the detector is invalid and reroll would misfire.

**Action for next agent / user:** on a **laggy** 1440p120 run, open the overlay and record
`Beat nz%` and `beats/win`. Compare against a smooth run.
- If laggy → low nz (or clearly different) → detector valid, build auto-reroll.
- If laggy → also high nz → nz does **not** discriminate; do NOT build nz-triggered reroll.
  Need a different bad-bootstrap signal (candidates: `Gap` distribution variance sampled
  finer, or an audio/video drift measure, or accept that only wall-clock A/B of a manual
  reload distinguishes — in which case reroll must self-validate by keeping the best of N
  reloads rather than thresholding nz).

## Agreed plan (user chose both; build order below)

### 1. Boot warmup (low risk — do first)
Do a throwaway NDL `load` + `drop` (unload) at app startup so the first real stream isn't
cold. Kills the deterministic "first stream always laggy."
- Where: app entry / `runtime::run` before the first `run_ui_flow`. `NdlVideo::load`
  needs `app_id` + a resolution/codec — use a modest default (e.g. 1920×1080 H264) purely
  to warm the decode graph, then drop immediately.
- **Risks to check:** `NDL_DirectMediaInit` is process-global (idempotent-guarded via
  `INIT_DONE`); a warmup load/unload must not leave the decoder holding resources that the
  first real load then fails on (see `docs/NOTES.md` — a failed load can hold decoder
  resources; always `Unload` after). Verify a warmup doesn't conflict with SDL video
  bring-up ordering. If a full load is too risky, test whether `ensure_init` alone (just
  `NDL_DirectMediaInit`) removes enough of the first-run cost.

### 2. Auto-reroll on bad bootstrap (invasive — only after the Open Question is settled)
Turn the coin-flip into "retry until heads": detect a bad bootstrap in the first ~800 ms
and **reload NDL in place** (unload+load, keeping the QUIC session), re-measure, repeat up
to ~3× until the cushion metric is good.

Design constraints discovered while scoping:
- **Ownership:** `player: VideoPlayer` moves into `video_pump` (thread). Reload logic lives
  inside `video_pump`; it must be able to reconstruct `NdlVideo::load(app_id, w, h, codec,
  audio_cfg)`. Those params are **not currently passed to `video_pump`** — thread the load
  params (or a reload closure) through `connect` → the spawn at `session/mod.rs:616`.
- **Audio-offload hazard:** when NDL audio offload is on, `ndl_audio_pump` holds an
  `Arc<NdlVideo>` clone and NDL unloads **process-globally** (see the `VideoPlayer` enum
  doc, `session/mod.rs:44`). Reloading video from the video thread while audio holds an Arc
  is unsafe — the old `NdlVideo` won't truly unload until the audio Arc drops. **Guard:
  only auto-reroll when NOT audio-offloaded** (the default config — offload is opt-in and
  off; `dev_override_enable_ndl_audio_offload`). Log-and-skip reroll when offloaded.
- **Resync after reload:** a reloaded decoder needs an IDR. Reuse the existing
  freeze-until-reanchor machinery — after reload set `holding = true`, call
  `client.request_keyframe()`, and let the existing reanchor path resume feeding on the
  next `FLAG_SOF`/recovery-anchor. Expect a brief black flash during reroll (acceptable —
  it happens in the first ~1–2 s before the user engages, same as a manual restart).
- **Teardown races:** recent commit #73 ("Bound session-teardown joins so a wedged NDL call
  can't freeze the app") touched NDL teardown. A mid-stream unload+load must respect those
  bounds; ensure the unload settles before the reload (the alternation parity in the
  observations *might* be an incomplete-unload-before-next-load race — worth a look even
  though the user deprioritised the standalone investigation).
- **Detector:** measure the cushion over the first ~600–800 ms (per-frame backlog mean or
  `nz%`), threshold decides reroll. Must run **regardless of overlay visibility** during
  bootstrap (the current per-frame sampling is overlay-gated) — add a bounded always-on
  bootstrap sampler, then fall back to the overlay-gated path.

### 3. If reroll works
Promote: consider making frame pacing **default-on** for high modes, and/or auto-tuning
`PRESENT_LEAD_FRAMES`. Trim the lead back toward the minimum that holds (each frame of lead
is added latency). Update `project_ndl_present_cushion_bootstrap` memory with the final
values and whether reroll made it deterministic.

## Current code state (all compiles: `task docker:check` green)

- `session/mod.rs`: `PRESENT_LEAD_FRAMES = 3`; lead applied in `video_pump`'s `play()` call.
  New `StreamStats` fields: `render_backlog_peak`, `feed_us_peak`, `frame_gap_min_us`,
  `frame_gap_max_us`, `ndl_buf_nonzero_pct`, `ndl_buf_beats`, `panel_hz`,
  `stats_overlay_shown`. Fast overlay-gated backlog/gap/beat sampling + 2 s decaying peaks.
  Heartbeat backlog log preserved (reuses fast sample when overlay up, else its own 0.5 Hz
  query — unchanged when hidden).
- `session/pacing.rs`: `panel_refresh_hz` made `pub`.
- `runtime/stream.rs`: overlay lines for panel Hz, buf peak, Gap, Beat; sets
  `stats_overlay_shown` each tick.
- `services/store.rs`: the `ndl-present-lead-ms.conf` dev-override was added then **removed**
  when the lead was hardcoded — do not expect it to exist.

Nothing for warmup or auto-reroll is written yet.
