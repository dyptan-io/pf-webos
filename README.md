# pf-webos

A native LG webOS TV client for [punktfunk](https://git.unom.io/unom/punktfunk) — low-latency
desktop and game streaming. Targets webOS 5.x+ (developed and verified live on an **LG CX,
webOS 5.6**), packaged as a homebrew `.ipk` and sideloaded via
[dev-manager-desktop](https://github.com/webosbrew/dev-manager-desktop).

This client is **based on and depends on** the [punktfunk](https://git.unom.io/unom/punktfunk)
project by **Enrico Bühler ([unom](https://unom.io))** — all credit for the protocol, FEC/crypto
core, and host implementation belongs there. This repo contains only the webOS-specific client:
an SDL2 UI, NDL DirectMedia hardware video decode, and webOS packaging, built directly on the
upstream `punktfunk-core` crate (pulled in as a pinned git dependency — see `Cargo.toml`).

## What it does

- Discovers punktfunk hosts on the LAN (mDNS) or lets you add one manually by IP.
- Pairs with a host via PIN, persists trust across restarts.
- Configurable resolution (1080p/1440p/4K), frame rate, bitrate (10-150 Mbps slider), and HDR.
- Shows the host's game library (if it has one) before streaming, so you can launch straight into
  a specific game instead of a plain desktop session.
- Hardware H.264/H.265 video decode via webOS's NDL DirectMedia API, audio via SDL2/PulseAudio.
- Magic Remote friendly: d-pad menu navigation, pointer/mouse hover+click, direct PIN/IP entry via
  the number pad, and the Red button as a reliable Back/disconnect substitute (see
  `docs/NOTES.md` — the hardware Back button is intercepted by webOS's system launcher).

## Building

All build/package logic lives in `Taskfile.yml` ([go-task](https://taskfile.dev)) — the same tasks
run locally, in Docker, and in CI, so there's only one place this logic is maintained.

**With Rust already installed** (macOS or Linux — the webosbrew NDK ships native builds for both):

```sh
task package   # fetches the toolchain (first run only, ~150MB), builds, packages
```

**With no local Rust/NDK at all — only Docker:**

```sh
task docker:package
```

Runs the whole pipeline inside an ephemeral `docker run --rm` against the stock `rust` image (no
custom Dockerfile). Caches (cargo registry/git, the webOS NDK, `target/`, `ares-package`) live in
named Docker volumes, so repeat builds are fast; only `dist/*.ipk` lands on your machine.

Either way, output is `dist/io.dyptan.punktfunk.webos_<version>_arm.ipk`. Run `task --list` for every
other task (`build`/`check` for a faster inner loop, `docker:shell` to debug inside the container,
`clean`/`clean:all`).

**Versioning**: the checked-in `appinfo.json`/`Cargo.toml` version stays a fixed `0.0.1` — webOS
itself never sees a "real" version. Every `.ipk`, dev or release, gets the HEAD commit's short sha
appended to its *filename* instead (e.g. `io.dyptan.punktfunk.webos_0.0.1+git.a1b2c3d4_arm.ipk`)
for traceability. The actual release version only ever shows up in the Homebrew Channel manifest
(`.github/workflows/build.yml`'s `release`-triggered job), generated from the GitHub Release tag.

## Installing on a TV (Developer Mode required)

```sh
task deploy TV_HOST=root@<tv-ip>   # build, package, install, and launch over SSH
task deploy:log TV_HOST=root@<tv-ip>   # tail the app's log afterward
```

Set `TV_HOST` once in a local `.env` (copy `.env.example`, gitignored) to skip typing it every
time. Logs go to `/tmp/punktfunk-webos.log` on the TV (also readable directly over plain SSH —
`/tmp` is shared between the app's jail and the host).

## Installing via Homebrew Channel

Once installed, updates and installs are one tap on the TV — no laptop/SSH needed:

1. Install [Homebrew Channel](https://www.webosbrew.org/) itself (one-time, standard webOS
   homebrew step).
2. Open Homebrew Channel → Configuration → Add repository, and enter:
   `https://raw.githubusercontent.com/dyptan-io/pf-webos/main/repo.json`
3. punktfunk now shows up in Homebrew Channel's app list, installable/updatable from there.

Only published [GitHub Releases](https://github.com/dyptan-io/pf-webos/releases) appear this
way — `repo.json` points at `.../releases/latest/download/...`, which only resolves once a
release is published (`.github/workflows/release.yml`), so dev/CI builds never show up here.

## Known platform limitations

Two things that look like client bugs are actually confirmed webOS/SDL-webOS limitations, not
fixable from application code — see `docs/NOTES.md` for the full research trail:

- **Frame rate only paces the stream, it doesn't change the TV panel's scan-out rate.** Neither
  `webosbrew/SDL-webOS` nor any webOS system service exposes a way for a native app to set the
  panel's actual refresh rate — only read it.
- **The Magic Remote's hardware Back button is intercepted by webOS's system launcher** before
  reaching any native app, in both menus and during streaming (a known upstream moonlight-tv
  issue, not specific to this client). The Red color button is used as the reliable substitute
  instead (short press = Back in menus, long press = disconnect during streaming).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), matching the upstream
punktfunk project, at your option.
