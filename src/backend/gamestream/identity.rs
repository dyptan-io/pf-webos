//! This device's `GameStream` identity, and the per-host certificate that pairing yields.
//!
//! Kept in separate files from punktfunk's `client-cert.pem` / `client-key.pem` even though
//! both are "this device's client certificate": punktfunk pins an Ed25519 QUIC identity,
//! `GameStream` needs RSA-2048 with a Moonlight-shaped subject, and the host keys its paired
//! list to the certificate. Reusing one file would mean re-pairing every host on the other
//! protocol the day either format changes.
//!
//! Trust runs the opposite way from punktfunk's. There, we pin the host's fingerprint and
//! the host approves us out of band. Here, pairing hands us the host's whole certificate,
//! and it is the only anchor for every later HTTPS request — so it is persisted per host,
//! not derived.
use std::path::PathBuf;

use anyhow::{Context, Result};
use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::http::pair::PairingCryptoBackend;
use moonlight_common::http::{ClientIdentifier, ClientSecret, ServerIdentifier};
use pem::Pem;

use crate::services::paths::app_dir;

fn client_paths() -> (PathBuf, PathBuf) {
    let dir = app_dir();
    (dir.join("gamestream-cert.pem"), dir.join("gamestream-key.pem"))
}

/// One host's certificate file. The address goes through a conservative filter rather than
/// into the name verbatim — an IPv6 literal has colons, and a hostname could carry a `/`.
fn server_path(address: &str) -> PathBuf {
    let safe: String = address
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    app_dir().join(format!("gamestream-server-{safe}.pem"))
}

fn read_pem(path: &PathBuf) -> Option<Pem> {
    let text = std::fs::read_to_string(path).ok()?;
    pem::parse(text).ok()
}

/// This device's `GameStream` client identity, generated on first use.
///
/// RSA-2048 keygen takes real time on this CPU (seconds, not milliseconds), which is why
/// this is a stored identity rather than a per-session one — and why callers must not run it
/// on the UI thread.
pub fn load_or_create_client() -> Result<(ClientIdentifier, ClientSecret)> {
    let (cert_path, key_path) = client_paths();
    if let (Some(cert), Some(key)) = (read_pem(&cert_path), read_pem(&key_path)) {
        return Ok((ClientIdentifier::from_pem(cert), ClientSecret::from_pem(key)));
    }

    tracing::info!("generating GameStream client identity (RSA-2048; this takes a moment)");
    let (identifier, secret) = RustCryptoBackend
        .generate_client_identity()
        .map_err(|e| anyhow::anyhow!("generate GameStream identity: {e}"))?;
    std::fs::write(&cert_path, identifier.to_pem().to_string()).context("write gamestream-cert.pem")?;
    std::fs::write(&key_path, secret.to_pem().to_string()).context("write gamestream-key.pem")?;
    Ok((identifier, secret))
}

/// The certificate `address` presented when we paired with it, if we ever did.
pub fn load_server(address: &str) -> Option<ServerIdentifier> {
    read_pem(&server_path(address)).map(ServerIdentifier::from_pem)
}

pub fn save_server(address: &str, server: &ServerIdentifier) -> Result<()> {
    std::fs::write(server_path(address), server.to_pem().to_string()).context("write gamestream server cert")
}

/// Drops the stored certificate for `address` — call after a host-side unpair, so the next
/// attempt starts from a clean pairing rather than presenting a certificate the host has
/// already forgotten.
pub fn forget_server(address: &str) {
    let path = server_path(address);
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("could not remove {}: {e}", path.display());
        }
    }
}
