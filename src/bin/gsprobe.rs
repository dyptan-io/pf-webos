//! Headless `GameStream` probe CLI (mirrors `pfprobe.rs`). Run on-device over SSH to exercise
//! the `moonlight-common` dependency against a real Sunshine host — P0 (does it link and run on
//! webOS armv7) and P1 (pairing and host queries) of `docs/GameStream-Plan.md`.
//!
//! ```text
//! gsprobe                          # P0: prove the crate linked
//! gsprobe info    <host> [port]    # /serverinfo
//! gsprobe pair    <host> [port]    # display a PIN, wait for the host's web UI
//! gsprobe applist <host> [port]    # /applist (requires pairing)
//! gsprobe art     <host> <app-id> [port]
//! gsprobe unpair  <host> [port]
//! gsprobe quit    <host> [port]    # /cancel whatever the host is running
//! ```
//!
//! The app is a single binary crate with no library target, so the modules under test are pulled
//! in by path rather than imported. The `#[path]`-on-an-inline-module form is what keeps their
//! `crate::…` paths resolving exactly as they do in the app: `gamestream::http` still finds
//! `services::pinned_tls` here. Compiling the real modules is the whole point — a probe that
//! reimplemented the HTTP or identity handling would prove nothing about what the app ships.
//!
//! The submodules are named one by one rather than pulled in through `backend/gamestream/mod.rs`
//! deliberately: that file holds the `HostBackend` impl, which reaches `crate::backend`,
//! `crate::core` and `crate::services::{discovery,library}` — the whole app. These three are the
//! `GameStream` code that stands alone, which is why the host calls live in `query.rs`.

#[path = "../services"]
mod services {
    // The probe spends only the HTTP budgets; the rest of the app's live in this file too.
    #[allow(dead_code)]
    pub mod budget;
    pub mod paths;
    pub mod pinned_tls;
}

#[path = "../backend/gamestream"]
mod gamestream {
    pub mod http;
    pub mod identity;
    pub mod query;
}

use anyhow::{Context, Result};
use gamestream::query;
use moonlight_common::AppId;

fn main() -> Result<()> {
    // Debug by default: `tracing-subscriber` is built without `env-filter` here, so `RUST_LOG`
    // does nothing and the default INFO ceiling would hide the per-request lines this exists for.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match argv.as_slice() {
        [] => link_check(),
        ["info", host, rest @ ..] => info(host, port(rest)?),
        ["pair", host, rest @ ..] => pair(host, port(rest)?),
        ["applist", host, rest @ ..] => applist(host, port(rest)?),
        ["art", host, app_id, rest @ ..] => art(host, app_id, port(rest)?),
        ["unpair", host, rest @ ..] => unpair(host, port(rest)?),
        ["quit", host, rest @ ..] => quit(host, port(rest)?),
        _ => {
            anyhow::bail!("usage: gsprobe [info|pair|applist|art|unpair|quit] <host> [args] — see the module docs")
        }
    }
}

fn port(rest: &[&str]) -> Result<Option<u16>> {
    match rest {
        [] => Ok(None),
        [p] => Ok(Some(p.parse().with_context(|| format!("bad port {p:?}"))?)),
        _ => anyhow::bail!("too many arguments"),
    }
}

/// P0: no host needed. Naming the types is what proves the pure-Rust AES/RSA path linked —
/// the half of the dependency with no C fallback to fall back on.
fn link_check() -> Result<()> {
    use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
    use moonlight_common::http::{DEFAULT_HTTPS_PORT, DEFAULT_HTTP_PORT};
    use moonlight_common::ServerVersion;

    // Sunshine reports 7.1.431 with a negative mini-patch; the parse is what tells us the
    // crate's version/server-type logic survived the cross-build with the right int widths.
    let version = ServerVersion::new(7, 1, 431, -1);
    println!(
        "moonlight-common ok: server_type={:?} version={}.{}.{} http={DEFAULT_HTTP_PORT} https={DEFAULT_HTTPS_PORT}",
        version.server_type, version.major, version.minor, version.patch
    );
    println!("crypto backend: {}", std::any::type_name::<RustCryptoBackend>());
    Ok(())
}

fn info(address: &str, port: Option<u16>) -> Result<()> {
    let host = query::open(address, port)?;
    println!("name:        {}", host.host_name().unwrap_or_default());
    println!("version:     {:?}", host.version().ok());
    println!("state:       {:?}", host.state().ok());
    println!("https port:  {:?}", host.https_port().ok());
    println!("paired:      {}", host.is_paired().unwrap_or(false));
    println!("current app: {:?}", host.current_game().ok());
    println!("mac:         {:?}", host.mac().ok().flatten());
    Ok(())
}

/// The flow is inverted from punktfunk's: this side generates the PIN and the *user* types it
/// into the host's web UI, so the PIN is printed before the blocking call rather than prompted
/// for. Sunshine shows a pending-PIN box; older `GameStream` hosts show a full-screen prompt.
fn pair(address: &str, port: Option<u16>) -> Result<()> {
    let host = query::open(address, port)?;
    if host.is_paired().unwrap_or(false) {
        println!("already paired with {address}");
        return Ok(());
    }
    let pin = query::generate_pin()?;
    println!("PIN: {pin}");
    println!("enter it in the host's web UI (Sunshine: Troubleshooting → PIN); waiting…");
    query::pair(&host, pin)?;
    println!("paired");
    Ok(())
}

fn applist(address: &str, port: Option<u16>) -> Result<()> {
    let host = query::open(address, port)?;
    for app in query::app_list(&host)? {
        let hdr = if app.is_hdr_supported { "  [HDR]" } else { "" };
        println!("{:>6}  {}{hdr}", app.id.0, app.title);
    }
    Ok(())
}

/// Ends the host's running session — the same call the host menu's "Close running app" makes, and
/// the way to check `<currentgame>` handling without a stream: run it while a game is up, then
/// `gsprobe info` should report `current app: Some(0)`.
fn quit(address: &str, port: Option<u16>) -> Result<()> {
    let mut host = query::open(address, port)?;
    match query::current_game(&host)? {
        None => println!("nothing running on {address}"),
        Some(app) => {
            println!("running app {}; cancelling…", app.0);
            println!("cancelled: {}", query::quit_running_app(&mut host)?);
        }
    }
    Ok(())
}

/// Writes the art next to the identity files rather than printing it — the point is to confirm
/// the authenticated *binary* endpoint works, which the text ones don't cover.
fn art(address: &str, app_id: &str, port: Option<u16>) -> Result<()> {
    let id: u32 = app_id.parse().with_context(|| format!("bad app id {app_id:?}"))?;
    let host = query::open(address, port)?;
    let bytes = query::box_art(&host, AppId(id))?;
    let path = services::paths::app_dir().join(format!("gamestream-art-{id}.png"));
    std::fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
    println!("{} bytes -> {}", bytes.len(), path.display());
    Ok(())
}

fn unpair(address: &str, port: Option<u16>) -> Result<()> {
    let host = query::open(address, port)?;
    query::unpair(&host)?;
    println!("unpaired from {address}");
    Ok(())
}
