# pf-webos

A native LG webOS TV client for [punktfunk](https://git.unom.io/unom/punktfunk) — low-latency
desktop and game streaming. Targets webOS 5.x+ (developed and verified live on an **LG CX,
webOS 5.6**), packaged as a homebrew `.ipk` and sideloaded via
[dev-manager-desktop](https://github.com/webosbrew/dev-manager-desktop).

Built on the upstream [punktfunk](https://git.unom.io/unom/punktfunk) project by **Enrico Bühler
([unom](https://unom.io))** — all credit for the protocol, FEC/crypto core, and host implementation
belongs there. This repo is only the webOS-specific client: an SDL2 UI, NDL DirectMedia hardware
video decode, and webOS packaging, built directly on the upstream `punktfunk-core` crate (a pinned
git dependency — see `Cargo.toml`).

## Features

- LAN discovery (mDNS) or add a host manually by IP; PIN pairing with persisted trust.
- Configurable resolution (1080p/1440p/4K), frame rate, bitrate, and HDR.
- Browses the host's game library (with cover art) and launches straight into a title.
- Hardware H.264/H.265 decode via webOS's NDL DirectMedia API; audio via SDL2/PulseAudio.
- Magic Remote friendly: d-pad navigation, pointer hover/click, number-pad PIN/IP entry, and the
  Red button as a Back/disconnect substitute (see `docs/NOTES.md` for why).

## Tasks

Everything is a [go-task](https://taskfile.dev) target (`Taskfile.yml`) — the same tasks run
locally and in CI, so there's only one place any of this is maintained. Run `task --list` for the
full list (including the `native:*`/`toolchain:*` internals these build on).

| Task                  | What it does                                                    |
| ---------------------- | ---------------------------------------------------------------- |
| `task package`         | Build + package `dist/*.ipk` — the one you usually want          |
| `task build` / `check` | Faster inner loop: just compile, or just `cargo check`           |
| `task lint` / `fmt`    | `cargo clippy` / `cargo fmt`                                      |
| `task deploy TV_HOST=root@<tv-ip>` | Build, package, install, and launch on a real TV over SSH |
| `task deploy:log TV_HOST=root@<tv-ip>` | Tail the app's log on the TV                         |
| `task shell`           | Interactive shell in the Docker build container (debugging)      |
| `task clean` / `clean:all` | Remove `dist/`, or everything (toolchain/target/Docker volumes) |

**Only Docker is required — no local Rust/NDK install needed.** The webOS cross-toolchain only
ships a Linux aarch64 build, so `build`/`check`/`package`/`lint` always run inside an ephemeral
`docker run --rm` (against the stock `rust` image, no custom Dockerfile) — this works the same on
an Intel/amd64 host too, via QEMU emulation. First run fetches the toolchain (~150MB); caches
(cargo, the NDK, `target/`, `ares-package`) live in named Docker volumes after that, so repeat
builds are fast. `fmt` runs natively (formatting doesn't need the cross toolchain). CI
(`.github/workflows/build.yml`) skips Docker and calls the `native:*` tasks directly, since its
runner is already Linux aarch64.

Set `TV_HOST` once in a local `.env` (copy `.env.example`, gitignored) to skip typing it every
time.

**Versioning**: `Cargo.toml`/`packaging/appinfo.json` stay a fixed `0.0.1` — webOS never sees a
"real" version. Every `.ipk` gets the HEAD commit's short sha in its *filename* instead (e.g.
`io.dyptan.punktfunk.webos_0.0.1+git.a1b2c3d4_arm.ipk`); the real release version only shows up in
the Homebrew Channel manifest, generated from the GitHub Release tag by
`.github/workflows/build.yml`'s `release`-triggered job.

## Installing

**Directly on a TV** (Developer Mode required):

```sh
task deploy TV_HOST=root@<tv-ip>
```

**Via Homebrew Channel** (updates/installs from the TV, no laptop needed):

1. Install [Homebrew Channel](https://www.webosbrew.org/) itself, if you haven't already.
2. Homebrew Channel → Configuration → Add repository →
   `https://raw.githubusercontent.com/dyptan-io/pf-webos/main/repo.json`
3. punktfunk now shows up in Homebrew Channel's app list.

Only published [GitHub Releases](https://github.com/dyptan-io/pf-webos/releases) show up this way
— dev/CI builds don't.

## Known platform limitations

Not fixable from application code — see `docs/NOTES.md` for the research trail:

- **Frame rate paces the stream only; it can't change the TV panel's scan-out rate.** No webOS
  system API exposes that to a native app.
- **The Magic Remote's hardware Back button is intercepted by webOS's system launcher by
  default** (a known upstream moonlight-tv issue too) — this client works around it (see
  `docs/NOTES.md`), with the Red button kept as a fallback for firmware where the workaround isn't
  honored.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), matching upstream
punktfunk, at your option.
