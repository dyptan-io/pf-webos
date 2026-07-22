# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Native LG webOS TV client for [punktfunk](https://git.unom.io/unom/punktfunk) (low-latency desktop/game
streaming). Targets webOS 5.x+, developed and verified live on an LG CX (webOS 5.6). Built directly on
`punktfunk-core` (pinned git dependency, see `Cargo.toml`) — deliberately *not* on upstream's
`pf-client-core` crate, whose Linux dependency table drags in FFmpeg/PipeWire/SDL3 (see `session.rs`
module docs).

Read `docs/NOTES.md` before touching video decode, rendering performance, the toolchain, or input
handling — it documents hard-won on-device findings and lists things *not* to re-attempt (e.g. reading
Back/Red via the safe SDL2 event API, an in-stream diagnostics overlay, blanket `clippy::pedantic`).

## Commands

Everything is a [go-task](https://taskfile.dev) target (`Taskfile.yml`); run `task --list` for the
full list. **Only Docker is required for local dev** — the webOS cross-toolchain only ships a Linux
aarch64 build, so `build`/`check`/`package`/`lint` run inside an ephemeral `docker run --rm` (works on
amd64 hosts too, via QEMU). First run fetches the toolchain (~150MB); cached in Docker volumes after
that. CI skips Docker and calls the `native:*` tasks directly (its runner is already Linux aarch64).

| Task | What it does |
| --- | --- |
| `task package` | Build + package `dist/*.ipk` — the one you usually want |
| `task build` / `task check` | Faster inner loop: just compile, or just `cargo check` |
| `task lint` / `task fmt` | `cargo clippy` (Docker) / `cargo fmt` (native) |
| `task deploy TV_HOST=root@<tv-ip>` | Build, package, install, and launch on a real TV over SSH |
| `task deploy:log TV_HOST=root@<tv-ip>` | Tail the app's log (`/tmp/punktfunk-webos.log`) on the TV |
| `task shell` | Interactive shell in the Docker build container (debugging) |
| `task clean` / `task clean:all` | Remove `dist/`, or everything (toolchain/target/Docker volumes) |

Set `TV_HOST` once in a local `.env` (copy `.env.example`) to skip typing it every time.

No test suite (no `#[test]`s) — verify via `task deploy` + `task deploy:log` on real hardware, or a
native `cargo check`/`cargo build` off-target as a quick syntax/type sanity check (macOS/Windows build
green via a stub `main()`, see below).

**Versioning**: `Cargo.toml`/`packaging/appinfo.json` stay a fixed `0.0.1` — every `.ipk` gets the HEAD
commit's short sha in its filename instead; the real release version only appears in the Homebrew
Channel manifest, generated from the GitHub Release tag (`.github/workflows/build.yml`).

## Architecture

**Platform gating**: almost the entire crate (`app`, `art`, `audio`, `discovery`, `gamepad`,
`keyboard`, `library`, `mouse`, `ndl`, `session`, `store`, `ui`, `wol`) is gated
`#[cfg(target_os = "linux")]` in `main.rs`. The webOS cross target (`armv7-unknown-linux-gnueabi`)
reports `target_os = "linux"`, same as a native Linux dev box. `main.rs` has a real `mod real` for
Linux and a stub (`anyhow::bail!`) otherwise, so `cargo build`/`check` stays green on macOS/Windows
without SDL2 installed.

**Two independent decode paths, no shared decode/present split**: video is hardware-decoded via
webOS's NDL DirectMedia API (`ndl.rs`) — one opaque call that decodes *and* presents, with no hook to
decode without displaying. Audio is decoded client-side via Opus (`audio.rs`) and played through plain
SDL2/PulseAudio; NDL's own audio path is unused. This matters for loss recovery: `session.rs`'s
`video_pump` reimplements a "freeze-until-reanchor" subset directly (holding frames back from
`ndl.play` during a gap until a real IDR or recovery-anchor frame arrives), since upstream
`punktfunk_core::reanchor::ReanchorGate` assumes a decode/present split this client doesn't have.

**Two runtime phases in `main.rs`, looped**: a pre-stream UI flow (`run_ui_flow`) and a streaming
event loop (in `run_inner`), alternating on `StreamOutcome::ReturnToMenu` vs. `Quit`.

- **Pre-stream UI** (`app.rs` + `ui.rs`): `app.rs` owns the screen state machine (`Screen::Home` —
  sidebar of known hosts + game grid — with `Pairing`/`Settings`/`AddHost`/`Wake`/`ForgetHost` as
  modals over it); `ui.rs` owns drawing primitives and key-to-`MenuEvent` mapping; `store.rs` owns
  persistence (identity/hosts/settings JSON); `discovery.rs` owns mDNS LAN discovery. Rendering goes
  through `ui::Painter` (`tiny_skia`-backed software framebuffer — no Skia/Vulkan/LVGL on webOS):
  `App::render` draws each screen into one `Painter` per dirty tick, `main.rs` uploads it to one
  persistent SDL2 texture and presents. Redraw-on-change (`dirty` flag), not unconditional-every-tick
  — this UI has no time-based animation, so pixels change only on an SDL event or a background
  discovery/art/library result.
- **Streaming** (`session.rs` + `ndl.rs` + `audio.rs` + `keyboard.rs`/`mouse.rs`/`gamepad.rs`):
  `session::connect` spawns the video pump thread (feeds decoded access units to NDL); audio is pumped
  from the *main* thread each tick instead (`AudioQueue` isn't `Send`); the input modules map SDL2
  events to punktfunk wire `InputEvent`s forwarded to the host live.

**Networking/auth is a trimmed, standalone port, not a shared dependency**: `discovery.rs` (mDNS) and
`library.rs` (mTLS game-library REST fetch, client-cert auth) mirror `pf-client-core`'s shape as
direct, independent implementations, again to avoid its dependency tree. `art.rs` fetches/decodes
cover art on a background thread into owned `tiny_skia::Pixmap`s, so nothing GPU-texture-shaped needs
cross-thread sync.

**Toolchain fragility** (full detail in `docs/NOTES.md`): `armv7-unknown-linux-gnueabi` defaults to
software-emulated floating point, not just a soft-float calling convention — `.cargo/config.toml`
overrides this (`-C target-feature=+neon,+vfp3,-soft-float`), the single highest-impact perf fix found
(~300ms → ~30ms per UI render). `src/glibc_compat_shim.c` + `build.rs` backfill `getauxval`/`gettid`/
`sendmmsg`, missing from webOS's ~2.12 glibc. SDL2 must be the `webosbrew/SDL-webOS` fork, not generic
SDL2 or the on-device system copy (too old) — the `.ipk` bundles its own `libSDL2-2.0.so.0`.
