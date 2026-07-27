# CLAUDE.md

Guidance for Claude Code working in this repo.

## What this is

Native LG webOS TV client for [punktfunk](https://git.unom.io/unom/punktfunk) (low-latency
desktop/game streaming). Targets webOS 5.x+, verified live on an LG CX (webOS 5.6). Built directly on
`punktfunk-core` (pinned git dep in `Cargo.toml`) — deliberately *not* on upstream `pf-client-core`,
whose Linux dep table drags in FFmpeg/PipeWire/SDL3.

**Read `docs/NOTES.md` before touching** video decode, rendering perf, the toolchain, or input — it
holds the on-device findings and the "don't re-attempt" list.

## Commands

All via [go-task](https://taskfile.dev) (`task --list`). **Only Docker is needed locally** — the
cross-toolchain is Linux-aarch64-only, so `build`/`check`/`package`/`lint` run in an ephemeral
`docker run` (amd64 works via QEMU). CI calls the `native:*` tasks directly.

| Task | What it does |
| --- | --- |
| `task package` | Build + package `dist/*.ipk` — the one you usually want |
| `task build` / `task check` | Faster inner loop: just compile, or just `cargo check` |
| `task lint` / `task fmt` | `cargo clippy` (Docker) / `cargo fmt` (native) |
| `task notices` | Regenerate `THIRD-PARTY-NOTICES.txt` (embedded in the About screen) |
| `task deploy TV_HOST=root@<tv-ip>` | Build, package, install, and launch on a real TV over SSH |
| `task deploy TV_HOST=... TELEMETRY=auto` | Same, but the app streams its logs (via `tracing`, see `src/logger.rs`) live to this machine over TCP instead of writing a file on-device — passed at launch via SAM's `params` (argv[1] JSON), no rebuild needed; `TELEMETRY=<dev-host>:<port>` for an explicit destination, `TELEMETRY_LEVEL=debug\|info\|warn\|error` (default `debug`) for the minimum level sent. The task listens locally first and prints lines as they arrive instead of returning. |
| `task shell` | Interactive shell in the Docker build container (debugging) |
| `task clean` / `task clean:all` | Remove `dist/`, or everything (toolchain/target/Docker volumes) |

Set `TV_HOST` in a local `.env` (copy `.env.example`). Run tasks with `task -s` to skip the command
echo. No test suite — verify via `task deploy` (with `TELEMETRY=auto` for live logs) on real
hardware, or a native `cargo check`/`build` (macOS/Windows stay green via a stub `main()`).


## Architecture

**Platform gating**: almost the whole crate (`app`, `art`, `audio`, `discovery`, `gamepad`,
`keyboard`, `library`, `mouse`, `ndl`, `session`, `store`, `ui`, `wol`, `compositor`) is
`#[cfg(target_os = "linux")]` in `main.rs` — the webOS target (`armv7-unknown-linux-gnueabi`) reports
Linux, same as a dev box. `main.rs` has a real `mod real` for Linux and an `anyhow::bail!` stub
otherwise, so `cargo build`/`check` stays green on macOS/Windows without SDL2.

**Two decode paths**: video is hardware-decoded via NDL DirectMedia (`ndl.rs`) — one opaque call that
decodes *and* presents, no decode-without-display hook. Audio is Opus-decoded client-side (`audio.rs`)
via SDL2/PulseAudio. This drives loss recovery: `session.rs`'s `video_pump` reimplements a
freeze-until-reanchor subset directly, since upstream `ReanchorGate` assumes a decode/present split
this client lacks.

**Two runtime phases** in `main.rs`, looped: pre-stream UI (`run_ui_flow`) and streaming
(`run_inner`), alternating on `StreamOutcome::ReturnToMenu` vs `Quit`.

- **Pre-stream UI** (`app/` + `ui/`): `app/` owns the screen state machine (`Screen::Home` —
  sidebar of known hosts + game grid — with `Pairing`/`Settings`/`AddHost`/`EditHost`/`Wake`/
  `HostMenu`/`WakeSettings`/`ForgetHost`/`About` as modals over it — `HostMenu` opens from the ⋯
  button on each sidebar host row, and `WakeSettings` from the ⋯ on its own "Wake host" row),
  one module per screen (`app/mod.rs` keeps
  the `App` struct, the shared drains, and the tile orchestration; its fields are `pub(crate)` so
  the screen modules can reach them). `ui/` owns drawing primitives and key-to-`MenuEvent`
  mapping, split along the same seams (`painter`/`text`/`rows`/`sidebar`/`grid`/`listmodal`/…) and
  glob-re-exported from `ui/mod.rs`, so everything is still reached as `crate::ui::X`.
  **Adding a screen**: if it's a list of actions, build it on `ui::ListModal` (see
  `app/hostmenu.rs` — rows + a Confirm arm, and the shell/focus-tile/animation machinery is
  shared); the `Screen` enum's eight dispatch sites are the only other edits. `store.rs` owns
  persistence (identity/hosts/settings JSON); `discovery.rs` owns mDNS LAN discovery. Rendering goes
  through `ui::Painter` (`tiny_skia`-backed software framebuffer — no Skia/Vulkan/LVGL on webOS):
  `App::render` draws each screen into one `Painter` per dirty tick, `main.rs` uploads it to one
  persistent SDL2 texture and presents. Redraw-on-change (`dirty` flag), not unconditional-every-tick
  — this UI has no time-based animation, so pixels change only on an SDL event or a background
  discovery/art/library result.
- **Streaming** (`session.rs` + `ndl.rs` + `audio.rs` + `keyboard.rs`/`mouse.rs`/`gamepad.rs`):
  `session::connect` spawns the video pump thread; audio is pumped from the *main* thread each tick
  (`AudioQueue` isn't `Send`); input modules map SDL2 events to punktfunk wire `InputEvent`s.

**Networking/auth is a standalone port**, not a shared dep: `discovery.rs` (mDNS) and `library.rs`
(mTLS game-library REST) mirror `pf-client-core`'s shape as independent impls to avoid its dep tree.
`art.rs` fetches/decodes cover art on a background thread into owned `tiny_skia::Pixmap`s.

**Toolchain fragility** (full detail in `docs/NOTES.md`): soft-float override in `.cargo/config.toml`
(the single biggest perf fix), glibc shims in `src/glibc_compat_shim.c` + `build.rs`, and the bundled
`webosbrew/SDL-webOS` fork. Don't re-derive these — read NOTES first.
