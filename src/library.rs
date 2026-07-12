//! Game-library fetch from the host's management REST API: `GET
//! https://<host>:<mgmt_port>/api/v1/library`, mTLS-authenticated by this device's
//! paired identity (no bearer token — the host authorizes by client certificate).
//! A trimmed port of `pf-client-core::library` (same wire shape, same mTLS pinning
//! verifier) rather than a dependency on that crate — see `session.rs`'s module docs
//! for why this client doesn't pull in `pf-client-core` at all.
use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

/// The management API's default port — matches the host's `mgmt::DEFAULT_PORT`. A
/// discovered host may advertise a different one via its mDNS `mgmt` TXT record
/// (`discovery::DiscoveredHost::mgmt_port`); saved-but-not-advertising hosts (or an
/// older host with no mgmt TXT at all) fall back here.
pub const DEFAULT_MGMT_PORT: u16 = 47990;

/// Cover-art paths for a title — each is a host-relative path (e.g.
/// `/api/v1/library/art/steam:570/portrait`), fetched through the same mTLS-pinned
/// management API `fetch_games` uses, not an external URL the client hits directly
/// (the host proxies Steam CDN/custom art itself — see `punktfunk-host::library`).
/// `art.rs` prefers `portrait` for the grid, falling back to `header`. `hero`/
/// `logo` mirror the host's full wire shape (a future detail/hero view could use
/// them) but nothing here reads them yet.
#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct Artwork {
    pub portrait: Option<String>,
    pub hero: Option<String>,
    pub logo: Option<String>,
    pub header: Option<String>,
}

/// One title in the host's unified library. `id` is store-qualified (`steam:<appid>`,
/// `custom:<id>`) and doubles as the launch handle `session::connect`'s `launch`
/// parameter takes — the host resolves the actual launch spec itself from `id`.
#[derive(Clone, Debug, Deserialize)]
pub struct GameEntry {
    pub id: String,
    pub title: String,
    /// `#[serde(default)]` so an older host that omits art still decodes — the grid
    /// just falls back to its placeholder card.
    #[serde(default)]
    pub art: Artwork,
}

/// Errors surfaced to the UI so it can explain what to do next.
#[derive(Debug)]
pub enum LibraryError {
    /// The host rejected our certificate — this device isn't on its paired list.
    NotPaired,
    /// The host's certificate didn't hash to the pinned fingerprint.
    PinMismatch,
    Http(u16),
    Unreachable(String),
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibraryError::NotPaired => f.write_str("Not paired — pair with the host first."),
            LibraryError::PinMismatch => f.write_str("Host certificate changed — re-pair with a PIN."),
            LibraryError::Http(code) => write!(f, "Management API returned HTTP {code}."),
            LibraryError::Unreachable(why) => write!(f, "Couldn't reach the host's management API: {why}."),
        }
    }
}

/// `https://addr:port`, IPv6 literals bracketed.
fn base_url(addr: &str, mgmt_port: u16) -> String {
    if addr.contains(':') {
        format!("https://[{addr}]:{mgmt_port}")
    } else {
        format!("https://{addr}:{mgmt_port}")
    }
}

fn agent(identity: &(String, String), pin: Option<[u8; 32]>) -> Result<ureq::Agent, LibraryError> {
    use rustls::pki_types::pem::PemObject;
    let bad = |what: &str, e: &dyn std::fmt::Display| LibraryError::Unreachable(format!("{what}: {e}"));
    // The ring provider, explicitly — the same one punktfunk-core's QUIC endpoints
    // install (via the `quic` feature), so the process never mixes rustls crypto
    // providers.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("tls config", &e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerify { pin }));
    let cert = rustls::pki_types::CertificateDer::from_pem_slice(identity.0.as_bytes())
        .map_err(|e| bad("client cert pem", &e))?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(identity.1.as_bytes())
        .map_err(|e| bad("client key pem", &e))?;
    let cfg = builder.with_client_auth_cert(vec![cert], key).map_err(|e| bad("client auth", &e))?;
    Ok(ureq::AgentBuilder::new()
        .tls_config(Arc::new(cfg))
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build())
}

/// Fetch the host's unified library. Errors are pre-classified for the UI (401/403 →
/// `NotPaired`, a pin-verifier rejection → `PinMismatch`).
pub fn fetch_games(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Vec<GameEntry>, LibraryError> {
    let agent = agent(identity, pin)?;
    let url = format!("{}/api/v1/library", base_url(addr, mgmt_port));
    let body = match agent.get(&url).call() {
        Ok(resp) => resp.into_string().map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?,
        Err(e) => return Err(classify(e)),
    };
    serde_json::from_str(&body).map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// Fetches one piece of cover art's raw bytes (JPEG/PNG, undecoded) from a
/// host-relative `art_path` (one of `GameEntry::art`'s fields) — same mTLS agent
/// as `fetch_games`. Decoding happens in `art.rs`, off this module's REST concern.
pub fn fetch_art(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    art_path: &str,
) -> Result<Vec<u8>, LibraryError> {
    let agent = agent(identity, pin)?;
    let url = format!("{}{art_path}", base_url(addr, mgmt_port));
    let mut buf = Vec::new();
    match agent.get(&url).call() {
        Ok(resp) => resp
            .into_reader()
            .read_to_end(&mut buf)
            .map_err(|e| LibraryError::Unreachable(format!("read art body: {e}")))?,
        Err(e) => return Err(classify(e)),
    };
    Ok(buf)
}

fn classify(e: ureq::Error) -> LibraryError {
    match e {
        ureq::Error::Status(401 | 403, _) => LibraryError::NotPaired,
        ureq::Error::Status(code, _) => LibraryError::Http(code),
        ureq::Error::Transport(t) => {
            let msg = t.to_string();
            if msg.contains("ApplicationVerificationFailure") || msg.contains("InvalidCertificate") {
                LibraryError::PinMismatch
            } else {
                LibraryError::Unreachable(msg)
            }
        }
    }
}

/// Fingerprint-pinning verifier — trust is the SHA-256 of the host's self-signed leaf
/// cert (via `punktfunk_core::quic::endpoint::cert_fingerprint`, the same hash the
/// QUIC session pinning uses), not a CA chain. The handshake signatures are still
/// verified for real: skipping that would let an active MITM replay the host's
/// (public) certificate and complete the handshake with its own key.
#[derive(Debug)]
struct PinVerify {
    pin: Option<[u8; 32]>,
}

impl rustls::client::danger::ServerCertVerifier for PinVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(expected) = self.pin {
            let fp = punktfunk_core::quic::endpoint::cert_fingerprint(end_entity.as_ref());
            if fp != expected {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure,
                ));
            }
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}
