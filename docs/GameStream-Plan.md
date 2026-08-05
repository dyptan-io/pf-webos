# Adding GameStream host support

Plan for supporting GameStream hosts (Sunshine / Apollo / Wolf) alongside punktfunk/1,
sharing one UI, one `Settings`, one NDL video path. **Pure Rust, no C linked.**

Scope is deliberately minimal: discover, pair, list games, stream video + audio + input.
Features with no GameStream analogue are hidden, not stubbed.

**One new setting, and only one:** `Settings::gamestream_enabled`, an off-by-default toggle on
the Experimental screen (`ui::EXP_ROW_GAMESTREAM`). With it off the client behaves exactly as it
does today — punktfunk only, no `_nvstream._tcp` browse, no manual-IP fallback probe, and any
already-paired GameStream `KnownHost` filtered out of the sidebar. The gate is purely runtime;
`moonlight-common` is always linked, so there is one build and one GPL-3.0 answer.

Reference C implementation studied for wire behaviour:
`../../misc/aurora-tv/core/moonlight-common-c`.

## Findings that shape the plan

### Encryption is not optional

| Channel | Mandatory | Cipher / key | Evidence |
| --- | --- | --- | --- |
| Control (ENet/UDP 47999) | **Yes** | AES-128-GCM, key = `rikey` from `/launch` | `ControlStream.c:321` — gated on `APP_VERSION_AT_LEAST(7,1,431)` alone, no feature flag, no opt-out. Sunshine reports 7.1.4xx. Plaintext packets on an encrypted stream are dropped (`ControlStream.c:1281`) |
| Input | **Yes** | folded into the control stream at ≥7.1.431 | `Limelight.h:94` "Remote input encryption is always enabled"; `SdpGenerator.c:190` sets `NVFF_RI_ENCRYPTION` unconditionally. No plaintext path exists at any version |
| Video | No — opt-in `ENCFLG_VIDEO` | AES-128-GCM | `VideoStream.c:95`. Host can force it via `x-ss-general.encryptionRequested` (`SdpGenerator.c:284-289`) |
| Audio | No — opt-in `ENCFLG_AUDIO` | AES-128-CBC | `SdpGenerator.c:193-197`. Host can force it likewise |
| RTSP (TCP 48010) | No | AES-128-GCM | Plaintext unless the host returns an `rtspenc://` session URL (`RtspConnection.c:955`) |

So: **request `ENCFLG_NONE`** for video and audio; control + input encryption we must
implement regardless. The total crypto surface for streaming is only AES-128-GCM and
AES-128-CBC — no hashing, RSA, HMAC, or KDF (`PlatformCrypto.h:27-28`). One quirk worth
noting: the GCM tag is written **before** the ciphertext, not after (`PlatformCrypto.c:76-102`).

Pairing is a separate, larger crypto story (AES-128-ECB, SHA-1/SHA-256 switch on server
major version, RSA-2048 self-signed cert, signature verify) — see `client.c:140-450`.

### Use `moonlight-common-rust`, do not port the C

A from-scratch port is ~5,000 lines of essential C (RTSP, SDP, ENet control, RTP video and
audio queues, NAL depacketizer, input transport) **plus** a bespoke Reed-Solomon decoder:
`nanors` builds a **Cauchy** parity matrix (`nanors/rs.c:100`), so the popular
`reed-solomon-erasure` crate is **not wire-compatible**. That is a 6-10 week project with a
long tail of host-compat quirks.

[`moonlight-common-rust`](https://github.com/MrCreativ3001/moonlight-common-rust) is a
pure-Rust implementation covering all of it plus the HTTP layer and the full pairing state
machine (`src/http/pair/phase1..phase5.rs`). Pinned as a git dependency — the same pattern
we already use for `punktfunk-core` — it eliminates the majority of the work.

Configuration for our target:

```toml
moonlight-common = { git = "https://github.com/MrCreativ3001/moonlight-common-rust",
                     rev = "<pinned sha>", default-features = false,
                     features = ["std", "stream-proto", "rustcrypto"] }
```

`default-features = false` matters: the default `ureq` feature pulls rustls → `ring`, which
needs a C compiler and per-arch asm. Dropping it also lets us reuse our existing `ureq` +
`rustls` stack through its `RequestClient` trait (`src/http/client/mod.rs`), keeping one HTTP
implementation in the app. `rusty_enet` and `fec-rs` are pure Rust, so nothing links C.

**Licensing.** It is GPL-3.0-or-later. `build.rs` will surface it in
`THIRD-PARTY-NOTICES.txt` automatically, but attribution and license propagation are
different things: linking GPL-3.0 code makes the distributed `.ipk` GPL-3.0 as a whole,
which is stricter than this repo's current MIT/Apache-2.0 dual license. This is a
deliberate, accepted trade for not writing 5,000 lines of protocol code. README notes it.
(For contrast, `moonlight-common-c` is LGPL-3.0 and would not propagate — but it is C.)

### Constraints inherited from the dependency

- **Sunshine-family hosts only.** Real NVIDIA GFE is explicitly unsupported. Acceptable:
  GFE is discontinued. But error messaging must say so rather than fail obscurely.
- **No reference-frame invalidation / LTR.** Loss recovery is IDR-request only. Our existing
  freeze-until-reanchor logic (`session/mod.rs:937-961`) still applies, keyed off frame
  index gaps rather than punktfunk's `FLAG_SOF` / `USER_FLAG_RECOVERY_ANCHOR`.
- **AV1 enumerated but unsupported.** `CodecPref` gains nothing; H.264 + HEVC only, which
  matches our existing setting exactly.
- **Nightly toolchain pin + edition 2024** upstream, and `fec-rs` is pinned with
  `features = ["parallel"]` unconditionally, which drags in `rayon` and a thread pool we do
  not want on a TV. Both likely require a fork. Budget for one.
- **Early project** — 50 commits, one author, unpublished, API churning. Pin a SHA, expect
  to hand-merge, and treat its ~180 unit tests as the reason to trust it at all.
- **Nobody has cross-compiled it to armv7.** Its CI is x86-64 only.

### The abstraction gets *easier* in pure Rust

The C route would have forced a push model on us: `submitDecodeUnit` calls the client from a
library thread. `moonlight-common-rust` exposes a **sans-IO** core
(`MoonlightStreamSetup::poll_output`, `src/stream/proto/mod.rs:166`) where we own the
sockets, the clock, and the loop. So we can drive it from our own pump thread and expose the
same **pull** shape `punktfunk_core::NativeClient` already has — `next_frame()`,
`next_audio()`, `send_input()`. Both backends then fit one symmetric trait with no
inversion-of-control adapter, and our PTS pacer keeps full control of presentation timing.

We use the sans-IO layer, not the batteries-included `stream::std::connect` driver: the
latter spawns its own thread and hardcodes `AudioConfig::STEREO`
(`src/stream/std/mod.rs:176`).

## Design

### Layout

```
core/protocol.rs      NEW  Protocol { Punktfunk, GameStream }, HostTrust
backend/              NEW  the abstraction; depends on core only
  mod.rs                   HostBackend, StreamSession, VideoSink, BackendCaps, registry
  punktfunk/               existing session/ + services/{discovery,library} logic, moved
  gamestream/              thin adapter over moonlight-common: mapping only, no protocol
session/              becomes backend-neutral: VideoSink impl, PtsPacer, StreamStats
```

`ui/`, `app/view/`, `platform/webos/*`, `services/{art,store,wol}` need no protocol
awareness at all. `services/art.rs` takes an injected fetch closure instead of calling
`library::fetch_art` directly.

### The traits

```rust
// backend/mod.rs
pub trait HostBackend: Send + Sync {
    fn protocol(&self) -> Protocol;
    fn caps(&self) -> BackendCaps;

    /// mDNS service type this backend owns. The type alone determines protocol —
    /// no probing for discovered hosts.
    fn discovery_service(&self) -> &'static str;
    fn parse_discovery(&self, info: &mdns_sd::ServiceInfo) -> Option<DiscoveredHost>;

    /// Liveness + pair-state. GameStream folds /serverinfo in here.
    fn probe(&self, host: &KnownHost, timeout: Duration) -> Result<HostStatus>;

    fn pair(&self, host: &KnownHost, pin: Pin4, cancel: &CancelToken) -> Result<HostTrust>;
    fn unpair(&self, host: &KnownHost) -> Result<()>;

    fn list_games(&self, host: &KnownHost) -> Result<Vec<GameEntry>>;
    fn fetch_art(&self, host: &KnownHost, e: &GameEntry, k: ArtKind) -> Result<Vec<u8>>;

    /// Launches (GameStream: /launch then RTSP) and returns a live session.
    fn connect(&self, spec: ConnectSpec, sink: Arc<dyn VideoSink>)
        -> Result<Box<dyn StreamSession>>;
}

pub trait StreamSession: Send {
    fn send_input(&self, ev: &InputEvent) -> Result<()>;
    fn next_event(&self, timeout: Duration) -> Option<StreamEvent>;
    fn next_audio(&self, timeout: Duration) -> Option<AudioPacket>;
    fn request_keyframe(&self);
    fn report_decode_us(&self, us: u64);      // no-op on GameStream
    fn stats(&self) -> &StreamStats;
    fn shutdown(self: Box<Self>) -> bool;     // false = wedged, skip ndl::quit()
}

pub struct BackendCaps {
    pub speed_test: bool,        // punktfunk only
    pub request_access: bool,    // punktfunk only (TOFU)
    pub host_abr: bool,          // punktfunk AIMD; false => resolve bitrate client-side
    pub hdr: bool,
    pub wake_on_lan: bool,
    pub cursor_capture: bool,
    pub unpair: bool,            // GameStream only (/unpair); punktfunk just forgets
}
```

Why this shape, and where the abstraction is stronger than the obvious version:

- **`VideoSink` is the single presentation path.** `trait VideoSink { fn submit(&self, au:
  &[u8], pts_ns: i64, flags: FrameFlags) -> SinkResult; }`, implemented **once** in
  `session/sink.rs` over `NdlVideo` + `PtsPacer` + `StreamStats`. Everything currently inline
  in `video_pump` (`session/mod.rs:840-1082`) moves here: PTS anchoring, the refresh-rate
  reconciled pacer, backpressure metering, hold-until-reanchor, and keyframe-request
  throttling. Both backends inherit all of it, and there is exactly one place that talks to
  NDL. `SinkResult::NeedKeyframe` is what makes this work for both — punktfunk's pump calls
  `request_keyframe()`, GameStream's returns `DecodeResult::NeedIdr`.
- **IDR throttling belongs in the sink, and must be raised for GameStream.** IDR requests
  travel the same TCP/ENet control channel as gamepad input, so an unthrottled request loop
  directly inflates input latency — this is the entire subject of
  `aurora-tv/idr-throttle-input-latency-v1.7.3.patch`. Our current
  `KEYFRAME_REQUEST_MIN_INTERVAL = 100ms` (`session/mod.rs:753`) should become a
  per-backend value, ~1000 ms for GameStream.
- **`HostTrust` replaces `fingerprint: [u8; 32]`.** That field is not protocol-neutral:
  punktfunk pins the host's self-signed leaf SHA-256, GameStream trusts a client cert pair
  and re-derives pair state from `/serverinfo`'s `PairStatus`. So
  `enum HostTrust { Pinned([u8; 32]), ClientCertPaired, Unpaired }`. Migration: `KnownHost`
  gains `#[serde(default)] protocol: Protocol` (defaults `Punktfunk`) and a one-shot upgrade
  in `store::load_known_hosts` maps an existing `Some(fp)` to `Pinned(fp)`.
- **`caps()` gates menus, never stubs.** `HostAction::SpeedTest` and the request-access
  button are absent for GameStream hosts rather than present-and-failing. This keeps the UI
  honest and means adding a third protocol later touches no view code.
- **`GameEntry.id` stays an opaque `String`.** Today `steam:<appid>`; for GameStream the
  decimal `<ID>` from `/applist`. `DESKTOP_PIN_ID` stays; resolving "desktop" to a
  GameStream app (the `Desktop` entry in `/applist`) happens inside the backend. No UI
  knowledge of either format.
- **`InputEvent` stays `punktfunk_core::input::InputEvent`.** It is already the app's internal
  event type across `keyboard.rs`, `mouse.rs`, `evmouse.rs`, `gamepad.rs`. Re-typing it would
  touch every platform module for no gain. The GameStream backend translates it to
  `ClientInputEvent` in `backend/gamestream/input.rs`; the only real cost is a Windows-VK
  mapping table. Moving `InputEvent` into `core::event` is a later, optional cleanup.
- **The registry is `fn backend_for(p: Protocol) -> &'static dyn HostBackend`.** Screens hold
  a `Protocol` (from `KnownHost`), never a concrete backend type.

### Settings: one new flag, everything else reused

`Settings` gains only `gamestream_enabled` (the Experimental gate above). Every stream setting
is reused as-is. Mapping:

| Setting | GameStream |
| --- | --- |
| `width`/`height`/`refresh_hz` | validated against `/serverinfo`'s `<DisplayMode>` list, then the `/launch` `mode` param |
| `bitrate_kbps` | `0` means host AIMD on punktfunk; on GameStream `caps().host_abr == false` so the backend resolves a resolution/fps-derived default |
| `hdr_enabled` | `/launch` `hdrMode=1` + HEVC Main10 format; forces limited color range |
| `codec: CodecPref` | H.264 / HEVC only — already exactly our two options |
| `audio_channels` | Opus multistream config; the sans-IO layer lets us pass 5.1/7.1 (the `std` driver would not) |
| `color_range_override` | `COLOR_RANGE_FULL` / `LIMITED`; ignored when HDR is on |
| `video_pacing`, `stats_overlay`, `show_logs`, `log_level_override` | shared `VideoSink` / logging, backend-independent |
| `gamepad_type`, `cursor_capture`, `game_mode` | platform-side, unchanged |

Fixed constants live in the backend, never in `Settings`: `packetSize = 1392`, `sops = 1`,
`fecPercentage`, `encryptionFlags = ENCFLG_NONE`.

### User-visible behaviour

**Discovery.** One `mdns_sd::ServiceDaemon` browses both `_punktfunk._udp.local.` and
`_nvstream._tcp.local.`. The service type alone sets `KnownHost.protocol` — no probing. For
GameStream, the SRV port is the HTTP port; `HttpsPort` comes from `/serverinfo`
(`client.c:977-1006`), defaults 47989 / 47984.

**Manual IP entry.** Unchanged UI. On confirm, try punktfunk pairing at `9777` first; on
failure, try GameStream. Show progress as a single "Connecting…" state so the fallback is
not exposed as an error. Once GameStream is detected, the pairing modal switches to the
GameStream layout.

**Pairing.** GameStream is **PIN-only** — no request-access / TOFU path. The flow is also
inverted from punktfunk: *we* generate the 4-digit PIN and display it for the user to type
into Sunshine's web UI, then poll. So `app/view/pairing.rs` gains a second layout
(display-PIN rather than enter-PIN), built on the same `ListModal` conventions. It carries a
warning line: GameStream host, reduced feature set. Timeout must be generous (aurora uses
60 s, `worker/pairing.c:18`) because a human is typing.

**Host sidebar section.** Shows the protocol for GameStream hosts. Actions filtered by
`caps()`: no Speed Test, no request-access. Gains Unpair (`/unpair`) alongside Forget, since
GameStream pair state lives on the host.

## Phases

Each phase compiles and ships. No long-lived branch.

**P0 — prove the dependency builds for armv7.** Add the pinned dep with
`default-features = false`, fork if the nightly pin, edition 2024, or `fec-rs`/`rayon` fight
our toolchain. A throwaway `src/bin/gsprobe.rs` (mirroring `src/bin/pfprobe.rs`) that calls
`MoonlightStreamSetup::new` and prints a version. Check `.ipk` size delta. **Nobody has
cross-compiled this crate; if P0 fails, stop and reconsider before anything else.**
*Exit: `gsprobe` runs on device.*

**P1 — pairing and host queries via `gsprobe`.** Wire `moonlight-common`'s HTTP + pairing
through our `ureq`/`rustls` stack via its `RequestClient` trait. Client identity persisted
alongside our existing `client-cert.pem` (separate files — different key type and subject).
Discover, pair, `/serverinfo`, `/applist`, `/appasset`. Zero UI, zero `App` changes.
*Exit: `gsprobe pair && gsprobe applist` works against a real Sunshine host.*

**P2 — introduce the seam, punktfunk-only, behaviour-neutral.** `core::protocol`,
`HostTrust`, `KnownHost.protocol` + store migration. Define the traits. Move existing code
into `backend/punktfunk/`. Extract pacer / hold / stats out of `video_pump` into
`session/sink.rs`. Replace direct `session::*` calls in `runtime/` and `app/state/` with
`dyn HostBackend`. Add `caps()` gating while only one backend exists. Wide but mechanical
diff; verify by streaming punktfunk and diffing the stats overlay.
*Exit: no punktfunk regression; `grep -r punktfunk_core src/app src/runtime` returns only
`InputEvent`.*

**P3 — GameStream backend, non-streaming half.** P1's code behind `HostBackend`. Dual mDNS
browse. Manual-IP fallback probing. The display-PIN pairing layout with its warning. Home
grid, art, protocol label and filtered host actions.
*Exit: a Sunshine host pairs, lists games with box art; connect returns "not implemented".*

**P4 — GameStream streaming.** Our own driver thread over the sans-IO core: UDP sockets,
the 500 ms video/audio ping obligation (`VideoStream.c` — this is what punches the firewall
and tells the host where to send), the 100 ms control ping (`ControlStream.c:1420`),
`poll_output` dispatch, frames into `VideoSink`. `InputEvent` → `ClientInputEvent`.
Opus into our existing `platform/webos/audio.rs`. `spawn_connect` becomes
backend-dispatched, with `/launch` inside `HostBackend::connect` so
`runtime/ui_flow.rs`'s hero-handover timing (`app.hero.handover_ready(...)`) is untouched.
*Exit: end-to-end desktop and game streaming on Sunshine, 4K60 HDR.*

**P5 — polish.** Termination-reason messages (`errors.rs` gains a GameStream taxonomy beside
`RejectReason`). Gamepad arrival/rumble. `<currentgame>` handling and Resume / Quit host
actions. webOS 5.1 `surroundParams = "642012345"` quirk (`session_worker.c:108-114`).

## Risks

1. **armby7 cross-compile of the dependency is unproven.** Highest-probability blocker, which
   is why it is P0. Fallback if it is unfixable: port the ~5,000 essential lines of LGPL-3.0 C
   ourselves (6-10 weeks, and needs a bespoke Cauchy GF(2⁸) Reed-Solomon decoder, since
   `reed-solomon-erasure` uses an incompatible Vandermonde matrix).
2. **GPL-3.0 propagates to the shipped `.ipk`.** Accepted, documented in README. Revisit only
   if distribution terms change.
3. **Dependency immaturity.** 50 commits, one author, unpublished, open issues including
   ">50000 kbps doesn't work" — directly relevant at 4K. Pin a SHA; expect a fork; its 180
   unit tests and `stream-c` A/B-against-C feature are the mitigation.
4. **Sunshine-family only.** Not a functional gap for us, but `probe` must detect a GFE host
   (`state` contains `MJOLNIR`, `client.c:917-937`) and say so plainly.
5. **P2 is the only phase that can regress punktfunk.** Behaviour-neutral by construction;
   the risk is the breadth of the diff, not its depth.
6. **Two mDNS browsers on one daemon.** Verify `mdns-sd` handles two service types without
   the per-host throttling aurora needed (`discovery/throttle.c`).
7. **Scope creep from GameStream-only concepts** — per-app HDR flags, a running-game state,
   quit-app semantics. Explicitly deferred to P5.

## Effort

| Phase | Size |
| --- | --- |
| P0 armv7 build | 1-3 days (fork risk dominates) |
| P1 pairing + host queries | 2-3 days |
| P2 seam refactor | 3-4 days, mechanical, wide diff |
| P3 non-streaming backend | 3-4 days |
| P4 streaming driver | 4-6 days |
| P5 polish | 2-3 days baseline |

~3 weeks, roughly a third of a from-scratch port. The dependency is doing the heavy lifting;
most of our work is the seam (P2) and the sans-IO driver (P4).
