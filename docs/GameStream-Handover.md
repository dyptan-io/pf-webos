# GameStream implementation — handover

State of the work described in `docs/GameStream-Plan.md`. Read the plan first; this file only
records what is *done*, what was *learned*, and what the next session should pick up.

Branch: `gamestream-seam`, one commit, not pushed. Base: `314d912`.

## Status by phase

| Phase | State |
| --- | --- |
| P0 armv7 build | **Done** at build level. Not run on device. |
| P1 pairing + host queries | Not started. |
| P2 seam refactor | **Half done.** Traits/protocol/trust/discovery/caps landed; `session/sink.rs` not started. |
| P3 non-streaming backend | Not started. |
| P4 streaming driver | Not started. |
| P5 polish | Not started. |

## What is verified, and how

Cross-target `task docker:check` and `task docker:lint` are green for
`armv7-unknown-linux-gnueabi`. Nothing has run on hardware — there is no `TV_HOST` in `.env`, and
Docker only cross-compiles.

**Nothing in this branch has been exercised at runtime.** The changes that most need a real run:

- Discovery still finds punktfunk hosts (the browse loop was rewritten from a blocking `recv` to
  a round-robin `try_recv`).
- An existing `known-hosts.json` migrates — paired hosts must still show as paired, and the file
  must lose its `fingerprint` keys on the next save.
- Pairing, library fetch with cover art, and a stream all still work.

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
- `src/bin/gsprobe.rs` — P0 probe.

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

## Next session: finish P2

The remaining half, and the only part that can regress punktfunk streaming.

Extract into `session/sink.rs`, out of `video_pump` (`src/session/mod.rs:840-1082`): host-PTS
anchoring, the refresh-rate-reconciled `PtsPacer`, backpressure metering, hold-until-reanchor,
and keyframe-request throttling. Target shape from the plan:

```rust
trait VideoSink { fn submit(&self, au: &[u8], pts_ns: i64, flags: FrameFlags) -> SinkResult; }
```

implemented **once** over `NdlVideo` + `PtsPacer` + `StreamStats`, so there is exactly one place
that talks to NDL and both protocols inherit the pacing work. `SinkResult::NeedKeyframe` is the
seam that serves both: punktfunk's pump calls `request_keyframe()`, GameStream returns
`DecodeResult::NeedIdr`.

Note while doing it: `KEYFRAME_REQUEST_MIN_INTERVAL` (`session/mod.rs:753`, 100 ms) must become
per-backend — roughly 1000 ms for GameStream, because IDR requests share the ENet control channel
with gamepad input, so an unthrottled request loop directly inflates input latency.

**Verify by streaming a punktfunk host and diffing the stats overlay against a build of
`314d912`.** Compile-clean is not evidence here.

Useful anchors found while exploring, for whoever does this:

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
  criterion of "runs on device" cannot be met without adding it to `packaging/`. The decision
  taken was to trust the clean cross-link and let P1 exercise the dependency against a real
  Sunshine host.
- The `dist/` `.ipk` at 4.29 MB was built from this branch, if you want a size baseline.
