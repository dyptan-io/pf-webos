# Handover: HID mouse input via evdev

Local working doc. Not for commit.

## Goal

A USB/dongle mouse plugged into the TV should feel like a mouse in a game — smooth, raw,
unbounded. Right now motion reaches us through webOS's compositor pointer (built for the
Magic Remote), and users report jitter in games.

## Why the current path is suspect

Motion arrives as SDL mouse events, which on webOS come from the compositor's own pointer.
Three separate problems stack up:

1. **Absolute quantization.** `mouse::move_event` sends `MouseMoveAbs` in panel space
   (`display_mode.w/h`, typically 4K). The host normalizes and maps into its output region
   (often 1080p), so two panel pixels collapse to one host pixel — a slow drag becomes
   0,1,0,1. Worst exactly where games are most sensitive.
2. **Edge clamping.** webOS's pointer cannot leave the screen, so the coordinate saturates
   while the mouse is still moving: sticking, then a jump. Also why the host cursor can't
   reach a second display.
3. **Compositor smoothing/resampling.** The pointer position is smoothed and resampled at
   the compositor's rate, built for a wrist-waved remote at ~60 Hz, not a mouse reporting at
   125–1000 Hz. Any deltas we derive are differenced from already-smoothed positions.

The branch `unbounded-captured-mouse` fixes (1) and (2) by switching to
`InputKind::MouseMove` deltas plus SDL relative mode when cursor capture is on. **It does not
fix (3)** — that needs bypassing SDL entirely, which is what this work is.

## How aurora-tv does it

`~/Sources/misc/aurora-tv` (fork of moonlight-tv). Their "Use Hardware Mouse" setting
(`hardware_mouse`) does exactly this bypass:

- `src/app/stream/input/session_evmouse.c` — dedicated SDL thread (`"sessinput"`), opens the
  device with `evmouse_open_default()`, blocks in `evmouse_listen(dev, mouse_listener, mouse)`.
  The listener translates evdev into SDL-shaped events and calls the normal handlers with
  `hw_mouse=true`.
- `src/app/stream/input/session_mouse.c:79` — with `no_sdl_mouse` set, motion **always** goes
  relative, from raw counts:
  ```c
  if (input->no_sdl_mouse) {
      if (!hw_mouse) { return; }
      LiSendMouseMoveEvent((short) event->xrel, (short) event->yrel);
  }
  ```
- `src/app/input/app_input.c:45` — `hardware_mouse` also suppresses
  `SDL_SetRelativeMouseMode` entirely; the two are mutually exclusive in the UI
  (`input.pane.c:96-105`).
- Buttons and wheel come off the same evdev thread, not SDL
  (`session_evmouse.c` `mouse_listener`, the `SDL_MOUSEBUTTONDOWN`/`SDL_MOUSEWHEEL` arms).

Note the `no_sdl_mouse` early return: while the evdev reader is live, SDL mouse events are
dropped. That's how they avoid double-sending, because **they do not grab the device** —
`EVIOCGRAB` appears nowhere in moonlight-tv master, and keko950's PR #559 that added it was
closed unmerged. Consequence: the compositor still sees the events and still draws its
pointer, which is why moonlight-tv issue #495 (LG cursor over the host cursor) is still open
for them.

`evmouse.c` itself lives in `mariotaku/commons-c` (`device/evmouse/evmouse.c`). **In your
checkout `third_party/commons` is an unpopulated submodule**, so the implementation was not
read — only its call sites. Fetch it before porting rather than reconstructing behavior.

## Blocker to resolve first

**Does the app jail expose `/dev/input/event*`?**

Do not start the port before answering this. Precedent is discouraging: `dualsense.rs`'s
module docs and `docs/NOTES.md:40` record that a webOS app's jail exposes **no `/dev/hidraw*`
node at all** — not with the pad connected, and with no `hidraw` class in `/sys` either
(verified webOS 10.3, non-rooted). That is why DualSense feedback goes through the Bluetooth
luna service instead. evdev is a different subsystem and may well be permitted where hidraw
isn't — aurora-tv ships this feature for dev-mode installs, which is real evidence it works —
but it is unverified here.

Cheapest check, before any design work:

```sh
# in a dev-mode shell, then again from inside the app's jail (the distinction matters —
# NOTES.md:68 records that jailed-uid behavior differs from an SSH shell)
ls -l /dev/input/
cat /proc/bus/input/devices
```

If the nodes exist but aren't readable by the app's uid, that's a different (possibly
solvable) problem than their not existing at all. `src/bin/pfprobe.rs` is the established
pattern for an on-device probe binary if a Rust-side check is easier to run than a shell one.

Outcomes:
- **Readable** → proceed with the design below.
- **Present but not readable** → check whether a dev-mode/root install differs; consider
  gating the feature on rooted TVs like `game_mode` does (`platform::webos::game_mode`).
- **Absent** → stop. The remaining lever is reducing compositor smoothing, or accepting
  relative-mode quality. Record the finding in `docs/NOTES.md` next to the hidraw one so
  nobody re-derives it.

## Design sketch

Assumes the blocker resolved favorably. Fits this codebase's layering (`CLAUDE.md`):
`platform/webos/` is the only place that may touch devices.

**New: `src/platform/webos/evmouse.rs`**

- Opens `/dev/input/event*`, picks devices advertising `EV_REL` with `REL_X`/`REL_Y`
  (mice), skipping absolute/touch devices. Log every candidate with its name — a bug report
  from an unknown dongle needs it.
- Reader thread (`std::thread::Builder::new().name("pf-evmouse")`), blocking reads of
  `input_event` structs. Send `InputEvent`s over an `mpsc` channel, matching how
  `dualsense.rs` structures its worker rather than inventing a new pattern.
- Translate: `REL_X`/`REL_Y` → accumulate within a `SYN_REPORT` frame, emit one
  `InputKind::MouseMove` per frame (don't send per-axis — that doubles packet count and
  splits diagonal motion). `REL_WHEEL`/`REL_HWHEEL` → `MouseScroll` via the existing
  `ScrollAccumulator`. `BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA` → `MouseButtonDown/Up` with
  `mouse::button_code`'s GameStream numbering (1=left, 2=middle, 3=right, 4=X1, 5=X2 —
  note evdev's `BTN_MIDDLE`/`BTN_RIGHT` ordering does **not** match).
- No `EVIOCGRAB` initially — see the trade-off below.
- Hot-plug: devices appear/disappear as dongles are plugged. Simplest workable version is a
  rescan on open failure plus a periodic retry; `inotify` on `/dev/input` is the tidier
  answer if that proves flaky.

**`src/runtime/stream.rs`**

- Start the reader at stream start when the feature is on, drain its channel each tick
  alongside audio (the loop already pumps audio from the main thread each tick — same place).
- **Drop SDL `Event::MouseMotion`/`MouseButton*`/`MouseWheel` while the reader is live**, or
  every movement goes to the host twice. This is aurora-tv's `no_sdl_mouse` early return and
  it is not optional.
- Stop and join the reader on all four stream-exit paths — the same paths
  `cursor.set_captured(false)` already covers. Bounded join, per the precedent in commit
  1db20a4 (wedged teardown must not freeze the app).

**Settings**

Add alongside `cursor_capture` (`core/model.rs`, `ui/settings.rs` — `ROW_CURSOR_CAPTURE` is
at index 7, rows are index-coupled so adding one shifts `ROW_EXPERIMENTAL` etc.). Prefer
detection over a toggle if a mouse can be identified reliably — `device.rs`'s doctrine is
"detection by attempt, not lookup table" — but a manual override is worth having when a
device is misdetected. aurora-tv makes hardware-mouse and absolute-mouse mutually exclusive;
mirror that or the two paths fight.

## Interaction with work already in flight

- **`MotionScaler` / `CAPTURED_MOTION_SCALE` (0.45) in `mouse.rs`** — damping added for the
  Magic Remote, whose deltas are coarse. **A desk mouse must not be damped by it**, or the
  evdev path will feel sluggish and the jitter fix will look like a regression. The scaler
  needs to be per-source: remote scaled, HID raw. Easiest is for the evdev path to call
  `mouse::move_relative_event` directly, bypassing `MotionScaler`.
- **`unbounded-captured-mouse`** (uncommitted at time of writing, stacked on
  `fix-webos-cursor-overlay`) — switches captured streams to relative. Land and test that
  first; it may reduce the jitter enough to change this work's priority.
- **`fix-webos-cursor-overlay` / PR #79** — the stray-cursor fix. Related: if we ever add
  `EVIOCGRAB`, the compositor stops seeing pointer events and its cursor has nothing to
  track, which would likely fix the overlay too. See trade-off below.

## The EVIOCGRAB trade-off

Grabbing (`ioctl(fd, EVIOCGRAB, 1)`) makes us the exclusive consumer:

- **For:** the compositor never sees the mouse, so it can't draw its pointer — plausibly a
  proper fix for the stray-cursor bug that PR #79 papers over with re-assertion. Also removes
  any need to filter SDL events, since none arrive.
- **Against:** exclusive means exclusive. If the reader thread dies, wedges, or the app is
  backgrounded without releasing, the user's mouse may be dead TV-wide until the process
  exits. Same class of global-state leak as the cursor visibility call, with worse symptoms.
  Also the Magic Remote and the mouse may share a device node — grabbing could take the
  remote with it, which would leave the user unable to navigate.

Recommendation: ship ungrabbed first (aurora-tv's shape, known to work), and only try
grabbing as a deliberate follow-up experiment with a guaranteed release path, once the basic
input path is proven.

## Verification

Cannot be validated off-device; see `feedback_debug_on_real_device_early`. `task deploy
TELEMETRY=auto` gives live logs.

Before writing the port, capture a baseline delta trace with the HID mouse — log `xrel`/`yrel`
and inter-event timing in the `Event::MouseMotion` arm at debug level, drag slowly and fast,
and read the shape:

- **alternating 0/1** → absolute quantization; `unbounded-captured-mouse` covers it, evdev may
  be unnecessary
- **irregular timing, smooth values** → compositor resampling; evdev is the fix, proceed
- **alternating sign/magnitude every other event** → the relative-mode warp emulation leaking
  synthetic events (SDL warps its pointer to screen centre each motion and suppresses the
  synthetic event with an internal `warping` flag; if that suppression is imperfect on this
  fork it shows up here)
- **plateaus then jumps** → edge clamping

Then, after the port: raw counts on the wire should match the mouse's DPI 1:1, jitter gone at
slow drag speeds, continuous camera rotation in a game, no double-movement (proves SDL
suppression works), and the mouse still usable after the stream and after killing the app
mid-stream.

## References

- `docs/NOTES.md:40` — no hidraw in jail. Read before assuming device access.
- `src/platform/webos/dualsense.rs:1-24` — precedent for "SDL can't reach the device, jail
  policy is the blocker", and for the worker-thread + channel shape.
- `src/platform/webos/mouse.rs` — existing wire mapping, `button_code` numbering,
  `ScrollAccumulator`, `MotionScaler`.
- punktfunk-core `input.rs:19` — `MouseMove` carries `dx`/`dy` in `x`/`y`.
- punktfunk-core `abi.rs:2439` — `PunktfunkCursorState` flags bit 1 is a host-driven relative
  hint ("a host app grabbed/hid the pointer"). The cursor channel is entirely unconsumed by
  this client; it's the protocol-correct way to switch absolute/relative automatically, and
  worth revisiting once raw input works.
- aurora-tv: `src/app/stream/input/session_evmouse.c`, `session_mouse.c:66-88`,
  `src/app/input/app_input.c:44-55`, `src/app/ui/settings/panes/input.pane.c:96-116`.
- `mariotaku/commons-c` `device/evmouse/evmouse.c` — the implementation to port.
- moonlight-tv issues [#495](https://github.com/mariotaku/moonlight-tv/issues/495) (cursor
  overlay with USB/dongle mice, open) and
  [#466](https://github.com/mariotaku/moonlight-tv/issues/466) (webOS 9 unresponsive cursor,
  firmware-fixed per user reports), PR
  [#559](https://github.com/mariotaku/moonlight-tv/pull/559) (EVIOCGRAB, closed unmerged).
