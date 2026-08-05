# GameStream implementation — handover

State of the work described in `docs/GameStream-Plan.md`. Read the plan first; this file only
records what is *done*, what was *learned*, and what the next session should pick up.

Branch: `gamestream-seam`, not pushed. Base: `314d912`.

## Status by phase

| Phase | State |
| --- | --- |
| P0 armv7 build | **Done** at build level. Not run on device. |
| P1 pairing + host queries | **Code complete, unproven.** No Sunshine host has been reached. |
| P2 seam refactor | **Done at build level.** Traits/protocol/trust/discovery/caps, plus `session/sink.rs`. The sink has not been streamed against a host. |
| P3 non-streaming backend | Not started. |
| P4 streaming driver | Not started. |
| P5 polish | Not started. |

## What is verified, and how

Cross-target `task docker:check` and `task docker:lint` are green for
`armv7-unknown-linux-gnueabi`. Nothing has run on hardware — there is no `TV_HOST` in `.env`, and
Docker only cross-compiles.

`gsprobe` was built and run for real, but only natively in the aarch64 container: the link
check prints the parsed `ServerVersion` and crypto backend, and `gsprobe info` against an
unroutable address fails cleanly with `Ureq(Timeout(Connect))`. That exercises argument
handling, the HTTP client construction and the error path — nothing that needs a host.

**No host, of either protocol, has been talked to.** The changes that most need a real run:

- Discovery still finds punktfunk hosts (the browse loop was rewritten from a blocking `recv` to
  a round-robin `try_recv`).
- An existing `known-hosts.json` migrates — paired hosts must still show as paired, and the file
  must lose its `fingerprint` keys on the next save.
- Pairing, library fetch with cover art, and a stream all still work.
- **The video sink.** Every frame now goes through `session::sink::NdlSink` instead of inline
  pump code. Stream a punktfunk host and diff the stats overlay (feed µs, backlog, pacing delta,
  hold behaviour on induced loss) against a build of `314d912`. Compile-clean is not evidence.
- **All of P1.** `gsprobe pair` against a Sunshine host is the phase's exit criterion and has
  not been run. The three places our own code could be wrong, in order of likelihood: the
  query-string assembly (upstream builds it through `hyper::Uri`, we assemble the string), the
  PKCS#8-vs-PKCS#1 private-key branch in `client_key_der`, and the exact-DER server verifier —
  a mismatch there presents as a TLS handshake failure right after pairing appears to succeed.

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

## The one new setting

`Settings::gamestream_enabled`, `#[serde(default)]` false, on the Experimental screen as
`ui::EXP_ROW_GAMESTREAM` (between Frame pacer and Game mode). Runtime gate only — no crate
feature, so there is one build and one GPL-3.0 answer.

**It is not yet enforced anywhere**, because nothing GameStream-related runs yet. When P3 lands,
it must gate at least: the `_nvstream._tcp` browse, the manual-IP fallback probe, and filtering
GameStream `KnownHost`s out of the sidebar.

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

**First, prove P1 against a real Sunshine host** — it is the cheapest way to find out whether
the HTTP client is right, and P3 builds directly on it. `gsprobe` is not in the `.ipk`
(see the loose ends below), so either add it to `packaging/` or run it from a dev-mode shell:

```text
gsprobe info <host>       # should print name, version, https port, paired=false
gsprobe pair <host>       # prints a PIN; type it into Sunshine's web UI
gsprobe applist <host>    # should list apps once paired
gsprobe art <host> <id>   # writes gamestream-art-<id>.png next to the identity files
```

Then P3 (the non-streaming backend): a `HostBackend` impl over `backend::gamestream`, the second
mDNS browse, manual-IP fallback probing, the display-PIN pairing layout, and the
`Settings::gamestream_enabled` gate — which is still enforced nowhere.

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
- **`gsprobe` is not shipped in the `.ipk`** (neither is `pfprobe`), so P0's stated exit
  criterion of "runs on device" cannot be met without adding it to `packaging/`. P1 now has
  real work for it to do on-device, so this is worth fixing rather than working around.
- The `dist/` `.ipk` at 4.29 MB was built from this branch, if you want a size baseline.
