# CLAUDE.md

## What this is

Native LG webOS TV client for [punktfunk](https://git.unom.io/unom/punktfunk) — low-latency desktop/game streaming. Targets webOS 5.x+. Built directly on `punktfunk-core` (pinned git dep), not upstream `pf-client-core` (drags in FFmpeg/PipeWire/SDL3).

**Read `docs/NOTES.md` before touching video decode, rendering perf, toolchain, or input** — it holds on-device findings and the "don't re-attempt" list.

## Commands

All via [go-task](https://taskfile.dev) (`task --list`). Only Docker needed locally (cross-toolchain is Linux-aarch64-only; CI runs `native:*` directly).

| Task | What it does |
| --- | --- |
| `task package` | Build + package `dist/*.ipk` |
| `task build` / `task check` | Compile release / quick `cargo check` |
| `task lint` / `task fmt` | `cargo clippy` (Docker) / `cargo fmt` |
| `task deploy TV_HOST=root@<tv-ip>` | Build, package, install, launch on TV over SSH |
| `task deploy ... TELEMETRY=auto` | Live app logs to this machine over TCP (no rebuild needed; see `src/logger.rs`). Set `TELEMETRY=<host:port>` or `TELEMETRY_LEVEL=debug\|info\|warn\|error` |
| `task shell` | Interactive Docker shell (debugging) |
| `task clean` / `task clean:all` | Remove `dist/`, or everything |

Set `TV_HOST` in `.env` (copy `.env.example`). Run with `task -s` to skip echo. No test suite — verify via `task deploy TELEMETRY=auto` on real hardware or native `cargo check` (macOS/Windows use stub).

`THIRD-PARTY-NOTICES.txt` regenerated automatically by `build.rs` on `Cargo.lock` changes.

## Architecture

**Platform gating**: almost the whole crate (`app`, `art`, `audio`, `discovery`, `gamepad`, `keyboard`, `library`, `mouse`, `ndl`, `session`, `store`, `ui`, `wol`, `compositor`) is `#[cfg(target_os = "linux")]` in `main.rs`. webOS target reports Linux same as dev box; macOS/Windows get stub `anyhow::bail!` in `main()` so builds stay green without SDL2.

**Two decode paths**: video via NDL DirectMedia (`ndl.rs`, opaque decode+present), audio client-side Opus (`audio.rs`). Loss recovery: `session.rs`'s `video_pump` reimplements freeze-until-reanchor directly since upstream `ReanchorGate` assumes decode/present split NDL lacks.

**Two runtime phases** in `main.rs`: pre-stream UI (`run_ui_flow`) and streaming (`run_inner`), alternating on `StreamOutcome::ReturnToMenu` vs `Quit`.

**Pre-stream UI** (`app/` + `ui/`): `app/` owns screen state machine (Home sidebar+grid, modals: Pairing/Settings/AddHost/EditHost/Wake/HostMenu/WakeSettings/ForgetHost/About), one module per screen. `ui/` owns drawing primitives and key→MenuEvent mapping. Add screen: build on `ui::ListModal` (see `app/hostmenu.rs`); `Screen` enum has eight dispatch sites (compiler finds them at once, mechanical but safe). `store.rs` persists identity/hosts/settings JSON; `discovery.rs` handles mDNS. Rendering: `tiny_skia` software framebuffer, redraw-on-change (`dirty` flag, no time-based animation).

**Streaming** (`session.rs` + `ndl.rs` + `audio.rs` + input modules): `session::connect` spawns video pump thread; audio pumped from main thread each tick; input modules map SDL2 events to `InputEvent`s.

**Networking**: `discovery.rs` (mDNS) and `library.rs` (mTLS game-library REST) are standalone impls (not via `pf-client-core`) to avoid its dep tree. `art.rs` background-fetches/decodes cover art.

**Toolchain notes**: soft-float override in `.cargo/config.toml` (biggest perf fix), glibc shims in `src/glibc_compat_shim.c` + `build.rs`, bundled `webosbrew/SDL-webOS` fork. Don't re-derive — read `docs/NOTES.md` first.

## Code comments

**Write only when necessary**: Do not write comments that repeat what the code obviously does. Write concise comments that explain WHY (non-obvious invariants, platform workarounds, subtle constraints).
