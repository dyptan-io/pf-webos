//! Native webOS TV client for punktfunk (see `docs/NOTES.md` for architecture).
//! Platform-gated to `target_os` = "linux" (both webOS and Linux dev boxes).
//
// `app`/`platform`/`session`/`runtime` are cfg-gated out on non-Linux hosts, so the `ui`/`core`/
// `services` items (and the glob re-exports feeding them) that they consume look "never used" on
// the macOS host build. Silence that there only — the Linux CI build still surfaces real dead code.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
#[cfg(target_os = "linux")]
mod app;
// Protocol seam: `core` + `services` only, no SDL2/webOS, so it stays ungated.
mod backend;
mod core;
mod errors;
mod logger;
#[cfg(target_os = "linux")]
mod platform;
mod services;
#[cfg(target_os = "linux")]
mod session;
mod ui;

#[cfg(target_os = "linux")]
mod runtime;

#[cfg(not(target_os = "linux"))]
mod runtime {
    pub fn run() -> anyhow::Result<()> {
        anyhow::bail!(
            "punktfunk-webos only runs under target_os = \"linux\" (a native Linux box, \
             or the armv7-unknown-linux-gnueabi webOS cross target) — see Cargo.toml"
        );
    }
}

/// Ceiling for `punktfunk-core`'s startup link-capacity probe, in kbps.
///
/// Core bursts at 2 Gbps by default, deliberately far above any plausible link so the burst
/// measures the link rather than itself. On a TV's Wi-Fi that backfires: measured on the G5,
/// three back-to-back connects split two ways — the two whose probe finished in ~1-2 s had
/// video within 2-4 s, the one that hit core's 6 s timeout showed **no video for 14 seconds**
/// (a black screen the user reads as a failed connect), and even a "successful" probe on that
/// link had the host dropping 20k packets.
///
/// 300 Mbps matches what this client already burst-tests its own speed probe at
/// (`session::run_speed_probe`, capped for the same reason: an unbounded firehose starves a
/// 2-3 core TV), and still sits well above the ~245 Mbps airlink ceiling this hardware can
/// reach — so it measures the link without knocking it over.
const ABR_PROBE_KBPS: &str = "300000";

fn main() -> anyhow::Result<()> {
    // Set before anything spawns a thread: `set_var` is not thread-safe, and core reads this
    // while building its data-plane pump during `connect`. An older core simply ignores it.
    std::env::set_var("PUNKTFUNK_ABR_PROBE_KBPS", ABR_PROBE_KBPS);
    runtime::run()
}
