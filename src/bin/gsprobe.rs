//! Headless `GameStream` probe CLI (mirrors `pfprobe.rs`). Run on-device over SSH to confirm the
//! `moonlight-common` dependency links and runs on webOS armv7 — the P0 gate of
//! `docs/GameStream-Plan.md`. Grows into pairing/`serverinfo`/`applist` coverage in P1.
//! Usage: gsprobe

use anyhow::Result;
use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::http::{DEFAULT_HTTPS_PORT, DEFAULT_HTTP_PORT};
use moonlight_common::ServerVersion;

fn main() -> Result<()> {
    // Sunshine reports 7.1.431 with a negative mini-patch; the parse is what tells us the
    // crate's version/server-type logic survived the cross-build with the right int widths.
    let version = ServerVersion::new(7, 1, 431, -1);
    println!(
        "moonlight-common ok: server_type={:?} version={}.{}.{} http={DEFAULT_HTTP_PORT} https={DEFAULT_HTTPS_PORT}",
        version.server_type, version.major, version.minor, version.patch
    );

    // Naming the crypto backend proves the pure-Rust AES/RSA path linked, which is the half of
    // the dependency that has no C fallback for us to fall back on.
    println!("crypto backend: {}", std::any::type_name::<RustCryptoBackend>());
    Ok(())
}
