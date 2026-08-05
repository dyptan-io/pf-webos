//! `GameStream` (Sunshine / `NVIDIA` `GameStream`) support.
//!
//! P1 of `docs/GameStream-Plan.md`: the host-query and pairing half, with no UI and no
//! `HostBackend` impl yet — `src/bin/gsprobe.rs` is the only caller. The plumbing is
//! deliberately thin: `moonlight-common`'s `MoonlightHost` does the protocol work, this
//! module supplies the HTTP stack ([`http`]) and the persisted identity ([`identity`]).
//!
//! Nothing here is gated on `Settings::gamestream_enabled`; the gate belongs at the points
//! where the *app* would start speaking `GameStream` (discovery browse, manual-IP probe,
//! sidebar filtering), which arrive with the `HostBackend` impl in P3.
use anyhow::{Context, Result};
use moonlight_common::high::std::MoonlightHost;
use moonlight_common::http::pair::PairPin;
use moonlight_common::http::DEFAULT_HTTP_PORT;
use moonlight_common::App;

pub mod http;
pub mod identity;

use self::http::GsHttpClient;

// The mDNS service type (`_nvstream._tcp.local.`) belongs beside `punktfunk::SERVICE_TYPE`
// when P3 adds the second browse — it has no caller before that.

/// How this device names itself in the host's paired-device list.
const DEVICE_NAME: &str = "webOS TV";

/// A host we can query. Thin alias so the `RequestClient` choice is stated in one place.
pub type Host = MoonlightHost<GsHttpClient>;

/// Opens a host at `address`, restoring the paired identity if this device has one.
///
/// Blocking: it fetches `/serverinfo` to learn the HTTPS port. `port` is the *HTTP* port
/// (the mDNS SRV port for a discovered host); the HTTPS one comes from the response.
pub fn open(address: &str, port: Option<u16>) -> Result<Host> {
    let host = Host::new(address.to_string(), port.unwrap_or(DEFAULT_HTTP_PORT), None)
        .map_err(|e| anyhow::anyhow!("open GameStream host: {e}"))?;

    if let Some(server) = identity::load_server(address) {
        let (client_id, client_secret) = identity::load_or_create_client()?;
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
        host.update()
            .map_err(|e| anyhow::anyhow!("query GameStream host {address}: {e}"))?;
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
    let (client_id, client_secret) = identity::load_or_create_client()?;
    host.pair(
        &client_id,
        &client_secret,
        DEVICE_NAME.to_string(),
        pin,
        moonlight_common::crypto::rustcrypto::RustCryptoBackend,
    )
    .map_err(|e| anyhow::anyhow!("pair: {e}"))?;

    let (_, _, server) = host
        .identity()
        .context("pairing reported success but left no server certificate")?;
    identity::save_server(host.address(), &server)?;
    tracing::info!("GameStream host {} paired", host.address());
    Ok(())
}

/// Drops the pairing on both sides: the host's `/unpair` endpoint, then our stored copy of
/// its certificate. The local half runs even if the host call fails — otherwise a host that
/// is offline (or has already forgotten us) leaves this device stuck presenting a
/// certificate nothing accepts.
pub fn unpair(host: &Host) -> Result<()> {
    let result = host.unpair().map_err(|e| anyhow::anyhow!("unpair: {e}"));
    identity::forget_server(host.address());
    result
}

/// The host's app list. Requires a paired host.
pub fn app_list(host: &Host) -> Result<Vec<App>> {
    host.app_list().map_err(|e| anyhow::anyhow!("app list: {e}"))
}

/// One app's box art (JPEG/PNG bytes, undecoded — `services::art` does the decode).
pub fn box_art(host: &Host, app_id: moonlight_common::AppId) -> Result<Vec<u8>> {
    host.request_app_image(app_id)
        .map_err(|e| anyhow::anyhow!("box art for app {}: {e}", app_id.0))
}
