# Handoff: webOS washed-out color investigation

## Symptom

punktfunk-host encodes NV12 BT.709 limited → NVENC H.264/8-bit, `hdr: false`, identical
pipeline for every client. iPad (VideoToolbox) always renders correct color. The webOS/TV
client is washed out — and, as of the latest repro, washed out on **both** decode sinks
(NDL and Starfish), consistently, once a confounding factor was removed.

## Timeline / ruled out

- Host-side pipeline confirmed byte-identical across sessions (H264, `client_video_caps=120`,
  bit_depth=8, Yuv420, NV12 BT.709 limited, NVENC 8-bit) — not a host bug.
- Initial NDL-vs-Starfish toggle looked like "NDL correct, Starfish washed out," but that was
  a **lingering display from a previous iPad session** still composited on the TV. Once that
  was cleared, both NDL and Starfish are washed out. **Only webOS is ever affected — iPad
  never is, on either sink.**
- HDR-on-client / SDR-stream combination goes *oversaturated* instead of washed out — same
  client, opposite direction, from a stream that's always `hdr: false`. Consistent with a
  colorimetry-signaling problem rather than a decode-pipeline bug.

## What the code review found (this session)

Both native decode paths **already** call the platform colorimetry API correctly, for both
HDR and SDR streams, with a comment describing this exact failure mode:

- `session.rs` (`connect`, ~line 396+) calls `player.set_color_info(...)` right after the
  decoder loads, forwarding `client.color` (`punktfunk_core::quic::ColorInfo`, itself decoded
  with safe BT.709-limited defaults if a legacy host omits the field — `Welcome::decode` in
  `punktfunk-core`).
- `ndl.rs::NdlVideo::set_color_info` → `NDL_DirectVideoSetHDRInfo` (transfer/primaries/matrix).
- `starfish.rs::StarfishVideo::set_color_info` → `StarfishMediaAPIs_setHdrInfo` JSON, which
  **does** include `videoFullRangeFlag`.

**One real platform gap**: NDL's native struct (`NdlHdrInfo`, `ndl.rs:111-128`) has **no
`full_range` field at all** — it's not part of the ST.2086 SEI struct NDL's API expects. NDL
genuinely cannot be told limited-vs-full range through this API. Starfish's JSON *can* carry
it, and does.

Given Starfish (which correctly signals range) is *also* washed out, the client-side
signaling code is very likely not the actual bug. Two remaining candidates:

1. The platform decoder/compositor firmware accepts the colorimetry call but doesn't honor it.
2. A TV-level/system picture setting (HDMI color range, "Vivid"/energy-saving picture mode,
   black-level control) overrides source signaling regardless of what any app sends —
   consistent with it affecting every sink identically and only on this TV/panel.

## New debug tooling (this session)

Built specifically to let someone test the above on real hardware without a rebuild per
experiment.

### 1. Settings → Color Range (Auto / Full / Limited)

`store.rs`'s `ColorRangeOverride`, wired into the regular Settings screen (`ui/settings.rs`,
`ROW_COLOR_RANGE`). Overrides `ColorInfo.full_range` right before every `set_color_info` call
in `session.rs` (initial connect + mid-session HDR re-sync). Takes effect on the **next
connect** — not live mid-stream.

- **On NDL this is a no-op** (see the platform gap above) — don't spend time testing NDL with
  this control, it can't change anything there.
- **On Starfish it should visibly change the picture.** If forcing "Limited" fixes Starfish's
  washout, that's strong evidence the stream (or the client's forwarding of it) is missing
  correct range signaling for that sink specifically. If it makes no visible difference
  either way, the bug is downstream of this API entirely (candidate 1 or 2 above).

### 2. Settings → Diagnostics → Log level

`app/diagnostics.rs`, reached from a "Diagnostics" row in Settings (not a hidden/remote-button
menu). Two rows: **Log level** — cycles Debug/Info/Warn/Error (Info on a fresh install),
applied **live** via a reloadable `tracing` filter (`logger.rs`) with no reconnect, and now
**persisted** across restarts (a `TELEMETRY_LEVEL` launch still overrides it for that run);
and the **Stats overlay** toggle (moved here from the main Settings list).

Set this to Debug before testing. The line to look for on every connect:

```
colour metadata sent: hdr=<bool> transfer=<u8> primaries=<u8> matrix=<u8> full_range=<u8>
```

(`session.rs`, right after the `set_color_info` call). `transfer=1 primaries=1 matrix=1
full_range=0` is correct BT.709 limited. If this ever shows something else with Color Range
left on Auto, that's the smoking gun and it's upstream of the client (host/negotiation).

### 3. Yellow button: log-tail overlay (works on every screen — menu and stream)

Cycles **Off → Live → Frozen → Off**:
- **Live**: tails the last ~12 log lines in a bar at the bottom of the screen, refreshed
  ~2Hz — lets you read the `colour metadata sent: ...` line without needing SSH.
- **Frozen**: freezes the current snapshot so you can actually read it (Live keeps scrolling).
- **Off**: hides it and frees the in-memory ring buffer (zero ongoing cost when unused).

### Fetching the full log afterward

```
scp root@<tv-ip>:/media/developer/apps/usr/palm/applications/io.dyptan.punktfunk.webos/punktfunk-webos-<version>.log ./
```

## Suggested next test pass

1. Diagnostics → Log level → Debug. Color Range → Auto. Connect via **Starfish**. Check the
   `colour metadata sent` line via the Yellow overlay (Frozen, to read it) — confirm it's
   `transfer=1 primaries=1 matrix=1 full_range=0`. Note whether the picture is washed out.
2. Same session (or reconnect), Color Range → Full, then → Limited. Reconnect between each
   (override applies at connect time). Does either setting change the picture on Starfish?
   - **Yes** → the bug is real range-signaling confusion somewhere upstream of this override
     point (host negotiation, or a `punktfunk-core` version-skew bug — note `Cargo.toml` is
     currently pinned to `punktfunk-core` `v0.21.0`, matching the host's reported `0.21.0`,
     so that specific skew theory is now ruled out for the client at least).
   - **No** → move to candidate 2: compare against **any other webOS app** on the same TV/HDMI
     input (YouTube, Netflix). If they *also* look washed out, it's a TV-level picture/HDMI
     range setting, not this client or punktfunk at all — check "HDMI ICC"/Color Range
     (Auto/Full/Limited) per-input and picture mode (Vivid/Standard/Energy Saving) in the
     TV's own settings.
3. If neither client-side override nor the TV's own picture settings change anything, the
   remaining candidate is the platform decoder/compositor silently ignoring the colorimetry
   call on this specific firmware — at that point it's worth filing upstream against
   webOS/NDL or Starfish rather than continuing to chase it client-side.
