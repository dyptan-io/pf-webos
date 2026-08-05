# GameStream implementation — handover

State of the work described in `docs/GameStream-Plan.md`. Read the plan first; this file only
records what is *done*, what was *learned*, and what the next session should pick up.

Branch: `gamestream-seam`, not pushed. Base: `314d912`.

## Status by phase

| Phase | State |
| --- | --- |
| P0 armv7 build | **Done** at build level. Not run on device. |
| P1 pairing + host queries | **Working against a real host.** Pairing completes, `applist` and box art work. Three client-side bugs found and fixed (query encoding, connection pooling, TLS resumption). See "What is verified". |
| P2 seam refactor | **Done at build level.** Traits/protocol/trust/discovery/caps, plus `session/sink.rs`. The sink has not been streamed against a host. |
| P3 non-streaming backend | **Code complete except the manual-IP fallback probe, unproven.** Backend, dual browse, display-PIN pairing, art, unpair, the `gamestream_enabled` gate. |
| P4 streaming driver | **Code complete, unproven.** No hand-rolled driver was needed — see below. |
| P5 polish | Not started. |

## What is verified, and how

Cross-target `task docker:check` and `task docker:lint` are green for
`armv7-unknown-linux-gnueabi`, and the three `gamestream::input` translation tests plus the
`EncodedQuery` one pass. `TV_HOST` **is** set in `.env` now (192.168.1.147) and a Sunshine host is on
the LAN at 192.168.1.102, so `task deploy PROBES=1 TELEMETRY=auto` is the loop to use — everything
below that says "unproven" can be tested.

`gsprobe` has been run **on the TV against the live Sunshine host**: `info`, `pair`, `applist` and
`art` all succeed there. It logs at debug unconditionally, because `tracing-subscriber` is built
here without `env-filter` and so ignores `RUST_LOG` — an empty trace from a probe run used to look
like a silent failure.

**A Sunshine host has now been talked to** (see the pairing findings below); **no punktfunk host
has**, so every P2 claim is still a compile-time claim. The changes that most need a real run:

- Discovery still finds punktfunk hosts (the browse loop was rewritten from a blocking `recv` to
  a round-robin `try_recv`).
- An existing `known-hosts.json` migrates — paired hosts must still show as paired, and the file
  must lose its `fingerprint` keys on the next save.
- Pairing, library fetch with cover art, and a stream all still work.
- **The video sink.** Every frame now goes through `session::sink::NdlSink` instead of inline
  pump code. Stream a punktfunk host and diff the stats overlay (feed µs, backlog, pacing delta,
  hold behaviour on induced loss) against a build of `314d912`. Compile-clean is not evidence.
- **Pairing from scratch with resumption disabled** — the pairing that completed did so one build
  earlier, on the retry path. See the P1 section below.

## P1: pairing against a real host

**Pairing works against a real Sunshine host** (192.168.1.102, with the TV at 192.168.1.147). Five
findings, in the order they surfaced — every one of them client-side:

1. **Fixed and proven: no request left the TV at all.** `moonlight-common` assembles queries through
   its `QueryBuilder` trait, and its `impl QueryBuilder for String` — the one we were passing —
   concatenates values verbatim, with a `TODO: filter for characters that need % serialization`
   where the encoding belongs. Our device name is `webOS TV`, so the space reached ureq and
   `http::Uri` rejected it: `InvalidUriChar` on the first phase. `GsHttpClient` now builds the query
   through its own `EncodedQuery` (percent-encodes everything outside RFC 3986 unreserved), which is
   what moonlight-qt does via `QUrlQuery` and what Sunshine decodes. Proven by the failure moving
   on. **Do not "simplify" this back to `String`.**
2. **Fixed: connection pooling.** The next failure was
   `Io(UnexpectedEof, "Peer disconnected")` about 8 ms after the request, **with nothing in
   Sunshine's log** — and the user had already entered the PIN, so it was not a timeout. Those three
   facts together say the bytes went into a socket the host had already closed and never reached its
   handler, which is what a stale pooled keep-alive connection does. Both agents now set
   `max_idle_connections(0)`; pairing is especially exposed because it has a human-scale pause (the
   PIN) in the middle, exactly when a pooled connection goes stale. The cost is one TCP/TLS setup
   per request, on a handful of one-shot LAN calls. The `UnexpectedEof` has not come back in any run
   since, though the failure it uncovered next (below) is what actually took the branch forward.
3. **Diagnosability.** Every pairing phase posts to `/pair`, so the failing phase was invisible. The
   request log line now carries the whole URL; read `phrase=` (`getservercert` / `clientchallenge` /
   `pairchallenge`) and the scheme to place any future failure. That line is what identified the
   next failure as phase 5.
4. **Fixed and proven: Sunshine cannot resume a TLS session.** With pooling off, the failure became
   `AlertReceived(InternalError)` on the phase-5 HTTPS call (`phrase=pairchallenge`) — the *only*
   HTTPS request in the handshake, sent right after phase 4 registers the client certificate. The
   alert comes from the host, and it is not about the certificate: the identical request with the
   identical certificate succeeds from the TV (`curl`) and from a desktop rustls client seconds
   later, and Sunshine had already accepted us (`/pair?phrase=pairchallenge` → `<paired>1</paired>`)
   while the TV had written no `gamestream-server-*.pem`. What Sunshine actually rejects is
   **resumption**: it hands out TLS 1.3 session tickets and then answers a resuming ClientHello with
   a fatal `internal_error`. Because nothing here pools connections, every HTTPS request after the
   first in a process tried to resume — two cached tickets, two dead handshakes, success only on the
   third attempt, which is the exact pattern the on-device trace showed for phase 5, `/serverinfo`
   and `/applist` alike. `with_certificates` now sets
   `cfg.resumption = rustls::client::Resumption::disabled()`. Proven: with it, `gsprobe applist` and
   a box-art fetch run with zero retries; without it, two per request.

   Two traps this cost time on, worth not repeating: `openssl s_client` and `curl` **cannot**
   reproduce it, because neither resumes — every hand-run reproduction attempt passed. And the
   symptom looks like a certificate problem, so the client-cert profile (`Profile::Root`, CA:TRUE
   with no `digitalSignature`), the key encoding, and the TLS version were all investigated and are
   all fine: Sunshine verifies the client certificate at the *HTTP* layer and answers an unknown one
   with a 401 body, never a TLS alert.
5. **Insurance, deliberately kept: `get_with_retry`.** Two retries 400 ms apart, only for errors
   where nothing was served (connect/handshake). Not the fix — resumption is — but pairing is the
   one flow whose failure costs the user a trip to the host for a fresh PIN, and it can leave the
   two sides disagreeing about whether they are paired, so one lost handshake should not end it.

**Pairing now completes** (`gsprobe pair` → `paired`, `gamestream-server-192.168.1.102.pem`
written), and `applist` and box art work against the live host. The pairing run happened one build
before resumption was disabled, so it went through on the retry path; the from-scratch pairing run
with the resumption fix in place is the one thing still owed. Everything else about P4 (the section
below) still has never had a frame through it.

**Running `gsprobe` over SSH: mind the file owner.** It runs as root, the app runs as uid 6703, and
both use the same app directory — a `gamestream-server-*.pem` written by the probe makes the app
fail with `Permission denied` on its next pairing. `chown 6703:5000` anything the probe leaves, or
delete it. Also copy the probe to `/tmp/gsprobe` rather than relying on the one in the package:
webOS reinstalls the app behind your back and the app directory's `bin/` loses it.

## Corrections to the plan

Things the plan assumed that turned out otherwise. Worth trusting these over the plan text.

- **No fork of `moonlight-common` was needed.** Its `rust-toolchain.toml` pins
  `nightly-2026-06-20`, but the crate contains no `#![feature(...)]` gates — the pin is cosmetic,
  and edition 2024 builds on the `rust:bookworm` stable in CI's image.
- **`fec-rs` does force `rayon`**, as the plan predicted, and it is not feature-gated upstream. It
  cross-compiles fine, so it is not a blocker; the open question is whether the thread pool costs
  anything on the TV, which is a P4 measurement, not a P0 one.
- **`mdns_sd::ServiceEvent::ServiceResolved` carries `Box<ResolvedService>`, not `ServiceInfo`.**
  `HostBackend::parse_discovery` takes `&mdns_sd::ResolvedService` accordingly.
- **`.ipk` size delta is not yet measurable.** 4.29 MB with the dep pinned, but no app code
  references `moonlight-common`, so LTO drops nearly all of it (`gsprobe` links to 471 KB). The
  real cost only shows up in P3/P4.
- **The plan's "no new user-facing settings" is void.** There is exactly one:
  `Settings::gamestream_enabled`.
- **P4 needs no hand-rolled stream driver.** `moonlight_common::stream::std::MoonlightStream` is
  one already; see the P4 section.

## P3: what landed, and what it still needs

The `HostBackend` impl and everything above it. `backend_for` is now **total**, so
`backend_or_punktfunk` is gone; `ALL_BACKENDS` has both entries and
`backend::browse_backends(gamestream_enabled)` is what discovery iterates.

- `src/backend/gamestream/query.rs` — the old `gamestream/mod.rs`, moved. It holds everything
  that needs only `moonlight-common` + our HTTP stack + stored identity, and `gsprobe` now names
  `http`/`identity`/`query` one by one instead of loading `mod.rs`. **Keep `crate::backend`,
  `crate::core` and `crate::app` out of `query.rs`** or the probe stops compiling.
- `src/backend/gamestream/mod.rs` — the seam: `SERVICE_TYPE` (`_nvstream._tcp.local.`), the
  `HostBackend` impl, and `GsArt`. `list_games` maps `/applist` to `GameEntry` with the decimal
  app id as both `id` and `art.portrait`.
- `HostBackend` gained three methods, each with a live caller: `default_query_port` (punktfunk's
  management port and `GameStream`'s HTTP port are different services, so the fallback can't sit
  at the call site), `unpair`, and `art_fetcher`.
- **`services::art` no longer knows how a cover is fetched.** `backend::ArtFetch` is the
  transport; `ArtRequest::paths` are handles the backend interprets. punktfunk's holds the
  `ureq::Agent` (`MtlsArt`), `GameStream`'s holds an open `MoonlightHost` (`GsArt`). Still built
  lazily inside the worker thread, so a fully cached library opens no connection — which is also
  why `ArtFetch` has no `Send` bound.
- **Pairing.** The display-PIN card says everything once, in the header: its subtitle carries the
  instruction ("…Enter this PIN on the host (Sunshine: Troubleshooting → PIN)") rather than
  repeating it as a second, centred caption above the digits. The remaining captions on the
  enter-PIN card wrap to the inner column like every other modal's body text, and both layouts
  measure them, so the rows below start in the right place whatever they wrap to.
- `Screen::Pairing` renders one of two layouts, chosen by
  `App::pairing_is_display_pin()` (the entry's protocol, deliberately *not* whether a PIN has
  been generated — a failed ceremony must keep its layout and error line). The display-PIN one
  has no focusable element at all, so it returns `None` from the focus-key/focus-rect paths and
  is skipped by both hover and click hit-tests. `open_pairing` starts the ceremony immediately
  for `GameStream`: nothing is for the user to type here.
- `PairingOutcome::result` is now `Result<HostTrust, String>` and it carries a `protocol`, so all
  three ceremonies land the right trust on a host `drain_pairing` may be saving for the first
  time.
- **Every `entries` rebuild goes through `App::rebuild_entries`**, which applies the
  `gamestream_enabled` filter. Five call sites collected the list themselves before; the filter
  would have been forgotten at one of them.
- `Settings::gamestream_enabled` is now enforced in three places: `browse_backends` (no
  `_nvstream._tcp` browse at all when off), `known_entries` (paired `GameStream` hosts hidden,
  not forgotten — the toggle is meant to be reversible), and it is read in `App::new` *before*
  the browse, which is why `load_settings` moved up.
- `task package PROBES=1` (and `docker:package`, `deploy`) now ships `pfprobe` and `gsprobe` in
  the `.ipk`. Off by default: it takes the package from **4.1 MB to 7.3 MB**, which the store
  build has no use for.

Deviations from the plan, same rule as the other lists — don't "fix" without reading:

1. **Unpair is folded into Forget, not a separate row.** The plan wanted both. A Forget that
   leaves the host still listing this device is simply wrong, and two rows that differ only in
   whether they also tell the host is a distinction users would have to be taught.
   `App::forget_host` fires `backend.unpair` on a worker when `caps().unpair` is set and does not
   wait: the local record goes either way, since a host that is offline must not block Forget.
2. **punktfunk's `unpair` is `Ok(())`, not a panic.** Discarding our pin *is* unpairing there, so
   "nothing left to do" is the true answer rather than an unsupported operation — even though
   the `caps()` check means it is never reached.
3. **`GameStream` hosts advertise no MAC**, so `DiscoveredHost::mac` is empty and the Wake row is
   absent for them. `/serverinfo` does carry one, but reading it would cost a round trip per
   discovery event; Wake-on-LAN for these hosts can wait for a reason to exist.
4. **`mgmt_port` is set to the SRV port** in `parse_discovery`, so one `GameStream` record
   carries the same port in both fields. Redundant, but it means a host saved from discovery
   never needs protocol-specific port defaulting later.
5. **The reduced-feature warning line is gone** from the display-PIN card (asked for
   mid-implementation). The host menu's subtitle still appends `· GameStream`.

**Still to do in P3: the manual-IP fallback probe.** It was left out on purpose. The plan assumed
manual entry pairs on confirm, but `App::confirm_add_host` only *saves* an address — nothing
probes, so there is no failure to fall back from. The two shapes that fit this codebase:

- Probe at add-host confirm, off-thread, only when the toggle is on: if `query::open` answers at
  47989 the record is rewritten to `GameStream` with port and `mgmt_port` 47989. Needs care so
  the punktfunk record at 9777 is *replaced*, not left beside it.
- Or probe at pair time: when punktfunk pairing fails on a manually-added host, try `GameStream`,
  flip the protocol, and re-open the modal in the display-PIN layout.

The first is smaller. Neither is worth writing blind — do it with a host in front of you.

## P4: what landed, and what it still needs

**The plan's central assumption for this phase was wrong, in our favour.** It budgeted a driver
thread over the sans-IO core: UDP sockets, the 500 ms video/audio ping obligation, the 100 ms
control ping, `poll_output` dispatch. `moonlight_common::stream::std::MoonlightStream` **is** that
driver — RTSP handshake, three `SyncUdpDriver`s, the `ENet` control peer, all ping obligations, on
its own threads — and it calls back into `VideoDecoder` / `AudioDecoder` / `ConnectionListener`.
So none of that was written here. `src/backend/gamestream/stream.rs` is the three callbacks plus
the settings mapping, and the protocol's timing stays in the crate that has tests for it.

- **Video** (`SinkDecoder`) owns an `NdlSink`, built in `setup` on the crate's video thread —
  the same reason `session::connect` builds punktfunk's on the pump thread (the pacer queries the
  panel refresh through SDL). `submit_decode_unit` concatenates the buffer chain into one Annex B
  access unit, derives the loss flag from a hole in `frame_number`, and answers
  `SinkResult::NeedKeyframe` with `DecodeResult::NeedIdr`. So the whole pacing/hold/backpressure
  path is shared with punktfunk, which is what P2's sink seam was for.
- **Audio** is software Opus, and cannot be decoded on the crate's audio thread:
  `sdl2::audio::AudioQueue` is `!Send` and lives on the main thread. Frames cross a bounded
  `sync_channel` (`try_send`, so a stalled main loop can't back the audio driver up) and are drained
  by `GsStream::pump_audio_once` on the same tick the punktfunk path uses. **Stereo only** — the
  software decoder's layout comes from punktfunk's `layout_for` (5 ms framing); `GameStream`'s
  surround configs need `OpusMultistreamConfig::from_surround_param`'s mapping, so `/launch` asks
  for stereo and `AudioDecoder::setup` refuses anything else rather than playing noise.
- **HDR** arrives on the control thread and goes straight to the shared `Arc<NdlVideo>`. It is
  **deduped** (`Shared::hdr_applied`): re-applying `NDL_DirectVideoSetHDRInfo` re-enters panel HDR
  mode and drops a 120 Hz panel to 60 — the CX finding in `docs/NOTES.md`. Note the **luminance
  unit conversion** in `hdr_meta`: `GameStream` sends max display luminance in whole nits, our
  `HdrMeta` (and NDL behind it) is 1/10000 cd/m². Passing it through unscaled would claim the
  content was mastered at 0.08 nits.
- **Input** (`gamestream/input.rs`) is a re-packing, not a re-mapping: punktfunk's input plane was
  modelled on `GameStream`'s, so button bits, stick range, trigger range, mouse button ids and
  wheel units are already identical. The two real differences are the `0x8000` keycode prefix and
  whole-pad gamepad state — hence the per-pad accumulator, the `ControllerConnect` before a pad's
  first state, and the modifier fold off the key stream. Three unit tests cover exactly those.
- **`runtime::stream_handle::StreamHandle`** is the seam the streaming loop now talks to, and it is
  an **enum, not the plan's `StreamSession` trait** — both implementations are in this build, so an
  enum keeps the compiler's totality check at each site rather than trading it for dynamic
  dispatch, and the two `shutdown`s have genuinely different teardown orders. `InputSender` is the
  one piece that is cloned out of it, for the HID-mouse reader thread.
- **`ConnectTarget` gained `protocol`** and its `fingerprint` became `Option`: only punktfunk has a
  host key to pin. `runtime::spawn_connect` dispatches on the protocol; the hero-handover timing in
  `ui_flow.rs` is untouched, because `/launch` happens inside the `GameStream` connect exactly as
  the plan asked.

Deviations and gaps, same rule as the other lists:

1. **No stream encryption** (`EncryptionFlags::NONE`). Optional on the video/audio planes, and this
   armv7 SoC has no hardware AES — the same reason punktfunk sessions ask for ChaCha20. Revisit only
   with a measurement.
2. **The Automatic bitrate is resolved client-side**, because `caps().host_abr` is false: Moonlight's
   60 Hz anchors (720p 10 / 1080p 20 / 1440p 40 / 4K 80 Mbps) scaled linearly in frame rate. This is
   the one figure no host tells us, so it has a test.
3. **`shutdown` waits on a flag, not a join.** The crate owns its threads, so teardown stops the
   stream, waits up to 2 s for the video callback's `stop` (which drops the sink's `Arc<NdlVideo>`),
   then releases our own handle — the last one, so `NdlVideo::drop`'s process-global unload cannot
   race a `play` still in flight. `false` skips `ndl::quit()`, same contract as punktfunk's.
4. **Pad feedback is not wired.** Rumble, triggers, LEDs and motion all arrive on the
   `ConnectionListener` and are P5; `StreamHandle::pump_feedback_once` is a no-op for `GameStream`.
   The overlay's Drop/FEC figures print `n/a` for the same reason — the protocol exposes no such
   counter to a client, and a zero would read as "no loss".
5. **Termination reasons are logged, not shown.** `ServerTermination`'s code lands in
   `Shared::termination_code` and nothing reads it yet; the taxonomy is P5's `errors.rs` work.

**None of P4 has had a frame through it** — it needs a paired host to reach, and pairing is still
open (see the P1 section). In likelihood order, the places to look first when it doesn't work: the
`/launch` settings the host rejects (`adjust_for_server` reports those with a reason), the `0x8000`
keycode prefix, the whole-pad state fold, and the HDR luminance conversion above.

## The one new setting

`Settings::gamestream_enabled`, `#[serde(default)]` false, on the Experimental screen as
`ui::EXP_ROW_GAMESTREAM` (between Frame pacer and Game mode). Runtime gate only — no crate
feature, so there is one build and one GPL-3.0 answer.

Enforced as of P3 in `browse_backends` and `known_entries` (see the P3 section above). The third
gate the plan calls for, the manual-IP fallback probe, arrives with that probe.

Toggling it takes effect on the **next launch**, because the mDNS browse is started once in
`App::new`. Restarting discovery live would mean tearing down and rebuilding the daemon from the
Experimental screen; it hasn't seemed worth it for a toggle flipped once.

## Map of what landed

- `src/core/protocol.rs` — `Protocol { Punktfunk, GameStream }`, `HostTrust { Unpaired,
  Pinned([u8;32]), ClientCertPaired }`.
- `src/core/model.rs` — `KnownHost` gains `protocol` and `trust`; `fingerprint` is gone, replaced
  by `KnownHost::pin()` / `is_paired()`. A private `legacy_fingerprint` field reads the old key
  and is `skip_serializing`. `KnownHost` now derives `Default` so literals can spread
  `..KnownHost::default()` rather than naming that storage detail.
- `src/services/store.rs` — `load_known_hosts` calls `migrate_legacy_trust` per record;
  `upsert_known_host` preserves existing `trust` when the incoming record is unpaired.
- `src/backend/mod.rs` — `HostBackend`, `BackendCaps`, `backend_for`, `backend_or_punktfunk`,
  `ALL_BACKENDS`.
- `src/backend/punktfunk/mod.rs` — pure delegation to `services::library` etc. `SERVICE_TYPE`
  is here now rather than inline in `discovery.rs`.
- `src/services/discovery.rs` — one daemon, one browse per `ALL_BACKENDS` entry, round-robin
  read; `DiscoveredHost` gains `protocol`.
- `src/services/library.rs` — `load_games_async` takes a `&'static dyn HostBackend`;
  `fetch_games` is now `pub(crate)` for the backend.
- `src/app/state/hostmenu.rs` — speed-test row gated on `caps().speed_test`.
- `src/app/state/pairing.rs` — default focus avoids the request-access button when
  `!caps().request_access`.
- `src/bin/gsprobe.rs` — the probe CLI: `info` / `pair` / `applist` / `art` / `unpair`, plus the
  bare P0 link check when run with no arguments.
- `src/backend/gamestream/mod.rs` — `open`/`pair`/`unpair`/`app_list`/`box_art` over
  `moonlight_common::high::std::MoonlightHost`. No `HostBackend` impl yet (P3).
- `src/backend/gamestream/http.rs` — `GsHttpClient`, our `RequestClient` over ureq + rustls.
- `src/backend/gamestream/identity.rs` — the RSA client identity and the per-host server
  certificate, in their own files beside punktfunk's.
- `src/services/pinned_tls.rs` — the ureq TLS transport, lifted verbatim out of
  `services::library` so both protocols share it. Each supplies its own verifier.
- `src/services/paths.rs` — `app_dir()`, moved out of `store` so `gsprobe` can pull in one leaf
  module instead of the persistence layer.
- `src/session/sink.rs` — `VideoSink`/`FrameFlags`/`SinkResult`, `NdlSink`, and `VideoPlayer`
  (moved out of `session/mod.rs`). See the sink section below.

## P1 notes

- **We implement `RequestClient` ourselves** (`backend/gamestream/http.rs`). The crate ships a
  ureq implementation, but it is behind a feature that also pulls in `hyper` (its URL builder
  returns a `hyper::Uri`), and it calls `.disable_verification(true)` under a
  `TODO: THIS MUST BE CHANGED`. Ours pins the server certificate by exact DER equality — there
  is no name or chain to check on a `GameStream` host — while still verifying handshake
  signatures, so a replay of the host's public certificate doesn't get through.
- **`gsprobe` reaches app modules with `#[path]` on inline `mod` blocks.** The crate has no
  library target, so a `src/bin/*.rs` cannot `use` the app's modules. The `#[path = "../backend"]
  mod backend { pub mod gamestream; }` form keeps their internal `crate::…` paths resolving
  unchanged, which is what makes the probe exercise the *shipping* code rather than a copy.
  If a library target ever appears, this collapses to plain imports.
- **`MoonlightHost` is used as-is.** It already caches `/serverinfo` and `/applist`, holds the
  client behind a mutex, and drives the five pairing phases. Wrapping it in our own state would
  duplicate that for nothing; P3's `HostBackend` impl should hold one per host.
- **The client identity is stored, not derived.** RSA-2048 keygen takes seconds on this CPU, so
  `identity::load_or_create_client` must never run on the UI thread — P3 needs it off-thread
  behind the pairing modal, the same way `App::try_request_access` already works.
- **`gsprobe art` writes the file rather than printing bytes.** The binary `/appasset` endpoint
  is the only authenticated non-XML path; the text endpoints wouldn't catch a break in it.

## Deliberate deviations from the plan's design

Do not "fix" these without reading the reasoning.

1. **`HostBackend` is narrower than the plan's version.** Only methods with a live caller exist:
   `protocol`, `caps`, `discovery_service`, `parse_discovery`, `list_games`. A trait method with
   one implementor and no caller is unverifiable scaffolding. `connect`, `pair`, `unpair`,
   `probe` and `fetch_art` should be added *with* their consumers. Same reasoning trimmed
   `BackendCaps` to `speed_test` and `request_access`; `host_abr` and `unpair` are noted in a
   comment where they'll go.
2. **`list_games` returns `LibraryError`, not a neutral error.** This leaks `services::library`
   into the trait. It has to, for now: `App::handle_library_error` opens the Wake dialog on
   `LibraryError::Unreachable` specifically, so flattening to `anyhow` would silently lose the
   Wake-on-LAN path. Replacing the taxonomy is the plan's P5 `errors.rs` work, and it should be
   done before P4 rather than after.
3. **`backend_for` returns `Option`.** `None` for `GameStream` until P3, so a hand-edited
   `known-hosts.json` is a visible dead end rather than a wrong-protocol connection. Call paths
   with no way to report that (a probe whose only output is a channel) use
   `backend_or_punktfunk`, which logs at error level first. Delete that helper once P3 makes
   `backend_for` total.

## The video sink (P2's second half)

`session/sink.rs` now owns everything between "an access unit arrived" and "NDL has been fed":
host-PTS anchoring, the reconciled `PtsPacer`, backpressure metering, freeze-until-reanchor, and
keyframe-request throttling. `video_pump` kept only the protocol-shaped parts — pulling frames,
deciding what counts as loss, and *how* a keyframe is asked for. It shrank by ~170 lines.

Deviations from the plan's sketch, same rule as the list above — don't "fix" without reading:

- **`submit` takes `&mut self`, not `&self`.** All the state it carries (pacer, anchor, hold
  deadline, throttle) is per-frame mutable and single-threaded by construction — the sink is
  owned by the pump thread. `&self` would only buy interior mutability nobody needs.
- **`pts_ns` is `u64`, not the plan's `i64`.** That's what core hands over, and negative host
  PTS has no meaning; `HostPtsAnchor::map` already floors at 0.
- **`FrameFlags` carries `index`.** Purely so the sink's warn/info lines can name the frame the
  way the inline code did. It is documented as logs-only.
- **`VideoSink` has `holding()` and `backlog()` beyond `submit`.** Both have live callers (the
  pump's 2 s heartbeat) — the "no unverifiable scaffolding" rule from the trait list above.
- **The keyframe throttle is per-sink config** (`SinkConfig::keyframe_min_interval`), not a
  constant. punktfunk passes `KEYFRAME_REQUEST_MIN_INTERVAL` (100 ms); GameStream should pass
  ~1000 ms, because IDR requests share the ENet control channel with gamepad input, so an
  unthrottled request loop directly inflates input latency.
- **`NdlSink` is built inside the video thread**, not at the `connect` call site: the pacer
  queries the panel refresh rate through SDL on construction, and that query was on the video
  thread before.

## Next session

**Nothing here has run against a host of either protocol.** P3 is written but every claim in it
is a compile-time claim. The order below is by how much it teaches per minute spent.

**1. Prove P1 with `gsprobe`, on device.** Still the cheapest way to find out whether the HTTP
client is right, and all of P3 sits on it. `task deploy PROBES=1 TV_HOST=root@<tv-ip>` puts it in
`/media/developer/apps/usr/palm/applications/io.dyptan.punktfunk.webos/bin/`:

```text
gsprobe info <host>       # should print name, version, https port, paired=false
gsprobe pair <host>       # prints a PIN; type it into Sunshine's web UI
gsprobe applist <host>    # should list apps once paired
gsprobe art <host> <id>   # writes gamestream-art-<id>.png next to the identity files
```

If pairing fails, the three likeliest places are still ours, in order: the query-string assembly,
the PKCS#8-vs-PKCS#1 branch in `client_key_der`, and the exact-DER server verifier — a mismatch
there presents as a TLS handshake failure right after pairing appears to succeed.

**2. Prove P3 through the UI**, with Experimental → GameStream on: the host appears in the
sidebar from the `_nvstream._tcp` browse, `Show pairing PIN…` displays four digits and completes
when they are typed into Sunshine, the grid fills with titles and one cover each, Connect reports
"not supported yet", and Forget drops the host from Sunshine's paired list too. Then turn the
toggle **off** and relaunch: the paired host must vanish from the sidebar and reappear when it
goes back on.

**3. Prove P2 has not regressed punktfunk**, which is still outstanding and is the only phase that
can: discovery, an existing `known-hosts.json` migrating (paired hosts still paired, `fingerprint`
keys gone after the next save), library with cover art, and **the video sink** — stream a
punktfunk host and diff the stats overlay (feed µs, backlog, pacing delta, hold behaviour on
induced loss) against a build of `314d912`.

**4. Prove P4**: with a paired Sunshine host, launch a title from the grid. Watch for
`GameStream launch ok`, then `NDL loaded for GameStream`, then a picture; check the stats overlay's
resolution/codec header and that Feed µs and the measured Mbps move. Then keyboard, mouse (both
captured and remote-pointer), and a pad — the pad is the one with real translation risk. HDR last,
and watch the panel's refresh rate: the dedupe in `set_hdr_mode` is what keeps 120 Hz.

**5. Finish P3**: the manual-IP fallback probe (see the P3 section for the two shapes and why it
was deferred), and P5's pad feedback + termination messages (see the P4 list).

`session::connect`'s argument list and the `Connected { client }` narrowing are leftover
P2-adjacent cleanups; neither blocks P3. The anchors found while exploring:

- `session::connect` is 15 positional args (`session/mod.rs:265-281`), sole caller
  `runtime::spawn_connect` (`runtime/mod.rs:47-97`). A `ConnectSpec` struct would collapse both
  `#[allow(clippy::too_many_arguments)]`s — the plan wants this anyway.
- `Connected` exposes `client: Arc<NativeClient>` publicly rather than wrapping `next_frame` /
  `send_input` / `request_keyframe`. Narrowing that field is the largest blast radius in the
  whole refactor; those verbs are used directly in `session/mod.rs` (`:911`, `:954`, `:1037`,
  `:1129`, `:1169`, `:1202`, `:1221`) and throughout `runtime/stream.rs`.
- Hero-handover timing (`runtime/ui_flow.rs:208-216`, `app.hero.handover_ready(...)`) must not
  move. The plan puts `/launch` inside `HostBackend::connect` precisely to keep it untouched.
- Identity is a bare `(String, String)` tuple threaded through `store` → `App` →
  `library::agent` → `art` → `session::connect`. A named `Identity` type is a low-risk cleanup if
  you're in there anyway.

## Loose ends unrelated to the refactor

- **`README.md` and `packaging/description.html` advertise GameStream in the present tense**, but
  nothing works yet — `description.html` is the Homebrew Channel store copy. Reword to
  planned/experimental, or hold both until P4.
- Size baselines from this branch: **4.1 MB** normally, **7.3 MB** with `PROBES=1`. Nothing in
  the app references `moonlight-common`'s streaming half yet, so LTO still drops most of it — the
  real cost lands in P4.
