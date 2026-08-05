//! The `GameStream` host-query and pairing calls: everything that needs only
//! `moonlight-common` plus our HTTP stack and stored identity, and nothing from the rest of
//! the app.
//!
//! That isolation is load-bearing. `src/bin/gsprobe.rs` pulls this file in by `#[path]`
//! alongside [`super::http`] and [`super::identity`] and *not* `gamestream/mod.rs`, which
//! reaches `crate::backend` for the seam and so cannot compile inside the probe. Keep
//! anything referring to `crate::backend`, `crate::app` or `crate::ui` out of here.
use anyhow::{Context, Result};
use moonlight_common::high::std::MoonlightHost;
use moonlight_common::http::pair::PairPin;
use moonlight_common::http::DEFAULT_HTTP_PORT;
use moonlight_common::App;

use super::http::{api_message, GsHttpClient};

/// Wraps a `moonlight-common` error into the one sentence a user can act on, logging the technical
/// form on the way — see [`api_message`]. Every host call below goes through it, so nothing carries
/// the crate's `{:?}`-formatted internals to a status line.
fn failed(what: &str, e: moonlight_common::high::MoonlightClientError) -> anyhow::Error {
    anyhow::anyhow!("{}", api_message(what, &e))
}

/// How this device names itself in the host's paired-device list.
const DEVICE_NAME: &str = "webOS TV";

/// A host we can query. Thin alias so the `RequestClient` choice is stated in one place.
pub type Host = MoonlightHost<GsHttpClient>;

/// Opens a host at `address`, restoring the paired identity if this device has one.
///
/// Blocking: it fetches `/serverinfo` to learn the HTTPS port. `port` is the *HTTP* port
/// (the mDNS SRV port for a discovered host); the HTTPS one comes from the response.
pub fn open(address: &str, port: Option<u16>) -> Result<Host> {
    let host =
        Host::new(address.to_string(), port.unwrap_or(DEFAULT_HTTP_PORT), None).map_err(|e| failed("open host", e))?;

    if let Some(server) = super::identity::load_server(address) {
        let (client_id, client_secret) = super::identity::load_or_create_client()?;
        // `set_identity` re-queries the host over HTTPS, so this is also the liveness check
        // and the "are we still on its paired list" check, in one round trip.
        match host.set_identity(client_id, client_secret, server) {
            Ok(()) => tracing::info!("GameStream host {address}: restored paired identity"),
            Err(e) => {
                // Not fatal: an unpaired-but-reachable host is a normal state, and the
                // caller may be about to pair. The stored certificate stays put — a host
                // that is merely offline must not cost us the pairing.
                tracing::warn!("GameStream host {address}: stored identity unusable ({e})");
            }
        }
    } else {
        host.update().map_err(|e| failed("query host", e))?;
    }
    Ok(host)
}

/// A four-digit PIN for [`pair`], generated here because `GameStream` inverts punktfunk's
/// flow: we display it and the user types it into the host's web UI.
pub fn generate_pin() -> Result<PairPin> {
    PairPin::new_random(&moonlight_common::crypto::rustcrypto::RustCryptoBackend)
        .map_err(|e| anyhow::anyhow!("generate PIN: {e}"))
}

/// Runs the five-phase pairing handshake and persists the host's certificate on success.
///
/// **Blocks for as long as the user takes to type the PIN** (the crate allows 90 s per
/// request). Never call it from the UI thread.
pub fn pair(host: &Host, pin: PairPin) -> Result<()> {
    let (client_id, client_secret) = super::identity::load_or_create_client()?;
    host.pair(
        &client_id,
        &client_secret,
        DEVICE_NAME.to_string(),
        pin,
        moonlight_common::crypto::rustcrypto::RustCryptoBackend,
    )
    .map_err(|e| failed("pairing", e))?;

    let (_, _, server) = host
        .identity()
        .context("pairing reported success but left no server certificate")?;
    super::identity::save_server(host.address(), &server)?;
    tracing::info!("GameStream host {} paired", host.address());
    Ok(())
}

/// Drops the pairing on both sides: the host's `/unpair` endpoint, then our stored copy of
/// its certificate. The local half runs even if the host call fails — otherwise a host that
/// is offline (or has already forgotten us) leaves this device stuck presenting a
/// certificate nothing accepts.
pub fn unpair(host: &Host) -> Result<()> {
    let result = host.unpair().map_err(|e| failed("unpair", e));
    super::identity::forget_server(host.address());
    result
}

/// The host's app list. Requires a paired host.
pub fn app_list(host: &Host) -> Result<Vec<App>> {
    host.app_list().map_err(|e| failed("app list", e))
}

/// The app the host is streaming right now, or `None` when it is idle.
///
/// `<currentgame>` is why launching a title that is already running resumes it instead of failing:
/// `MoonlightHost::start_stream` reads this and posts `/resume` rather than `/launch`. So this
/// client needs it only to tell the user what is running, and to decide whether quitting has
/// anything to quit.
pub fn current_game(host: &Host) -> Result<Option<moonlight_common::AppId>> {
    let id = host.current_game().map_err(|e| failed("current game", e))?;
    Ok((id != 0).then_some(moonlight_common::AppId(id)))
}

/// Ends the host's running session, whoever started it (`/cancel`).
///
/// `false` means nothing was ended: either the host was idle, or the session belongs to another
/// device and the host refused — the two are worth distinguishing to the user, but the endpoint
/// does not, so the caller reports one sentence for both.
pub fn quit_running_app(host: &mut Host) -> Result<bool> {
    host.cancel().map_err(|e| failed("quit running app", e))
}

/// One app's box art (JPEG/PNG bytes, undecoded — `services::art` does the decode).
pub fn box_art(host: &Host, app_id: moonlight_common::AppId) -> Result<Vec<u8>> {
    host.request_app_image(app_id).map_err(|e| failed("box art", e))
}
