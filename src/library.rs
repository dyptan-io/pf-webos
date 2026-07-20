//! Game-library fetch from the host's management REST API: `GET
//! https://<host>:<mgmt_port>/api/v1/library`, mTLS-authenticated by this device's
//! paired identity (no bearer token — the host authorizes by client certificate).
//! A trimmed port of `pf-client-core::library` (same wire shape, same mTLS pinning
//! verifier) rather than a dependency on that crate — see `session.rs`'s module docs
//! for why this client doesn't pull in `pf-client-core` at all.
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, TcpConnector, Transport, TransportAdapter,
};

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
            Self::NotPaired => f.write_str("Not paired — pair with the host first."),
            Self::PinMismatch => f.write_str("Host certificate changed — re-pair with a PIN."),
            Self::Http(code) => write!(f, "Management API returned HTTP {code}."),
            Self::Unreachable(why) => write!(f, "Couldn't reach the host's management API: {why}."),
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

/// Builds one mTLS `ureq::Agent`, reusable across many requests to the same host
/// (`ureq::Agent` owns a connection pool — reusing one instance keeps a TCP+TLS
/// connection alive across calls instead of paying a fresh mutual-TLS handshake,
/// including re-parsing the PEM identity, every single time). Exposed so
/// `art.rs`'s per-game cover-art loop can build it once outside the loop, instead
/// of what `fetch_art` used to do (build a fresh one — and a fresh handshake —
/// per game, which is real, avoidable latency/CPU cost for a library of any size).
pub fn agent(identity: &(String, String), pin: Option<[u8; 32]>) -> Result<ureq::Agent, LibraryError> {
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
    let cfg = builder
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| bad("client auth", &e))?;

    // ureq 3.x's own `TlsConfig`/`RustlsConnector` only build a `rustls::ClientConfig`
    // from a fixed CA-chain/platform-verifier menu — no hook for `cfg`'s custom
    // fingerprint-pinning `PinVerify` above. So we skip that layer entirely and hand
    // `cfg` to our own minimal TLS-wrapping `Connector` (`PinnedTlsConnector` below,
    // modeled on ureq's own `RustlsConnector`), chained onto the stock `TcpConnector`.
    let connector = TcpConnector::default().chain(PinnedTlsConnector { config: Arc::new(cfg) });
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(10)))
        .build();
    Ok(ureq::Agent::with_parts(config, connector, DefaultResolver::default()))
}

/// Fetch the host's unified library. Errors are pre-classified for the UI (401/403 →
/// `NotPaired`, a pin-verifier rejection → `PinMismatch`). Only called from
/// `load_games_async` — `App` never calls this directly on the UI thread (see that
/// function's docs on why).
fn fetch_games(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Vec<GameEntry>, LibraryError> {
    let agent = agent(identity, pin)?;
    let url = format!("{}/api/v1/library", base_url(addr, mgmt_port));
    let body = match agent.get(url.as_str()).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?,
        Err(e) => return Err(classify(e)),
    };
    serde_json::from_str(&body).map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// One `fetch_games` call's result, delivered over `load_games_async`'s channel —
/// carries `host`/`port`/`mgmt_port` back alongside the result since the receiving
/// end (`App::drain_games`) needs them to start art loading on success, without
/// having to keep its own copy in sync with whatever host is selected by the time
/// the fetch completes.
pub struct GamesLoaded {
    pub host: String,
    pub port: u16,
    pub mgmt_port: u16,
    pub result: Result<Vec<GameEntry>, LibraryError>,
}

/// Spawns one background thread to run `fetch_games` and deliver its result —
/// `agent(...).get(...).call()` blocks on a real network round-trip (up to the
/// 5s connect / 10s total timeout `agent` sets), so calling `fetch_games` directly
/// from the UI thread (the old behavior) froze every input — button presses,
/// pointer motion, rendering — for as long as the host took to answer or time out.
/// Switching hosts again before this finishes is safe: `App::select_host` replaces
/// `games_rx` with a fresh channel, dropping this one's receiver, so this thread's
/// `tx.send` just fails and it exits — the same discard-on-drop pattern
/// `art::load_art_async` already relies on.
pub fn load_games_async(
    host: String,
    port: u16,
    mgmt_port: u16,
    identity: (String, String),
    fingerprint: Option<[u8; 32]>,
) -> std::sync::mpsc::Receiver<GamesLoaded> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("punktfunk-webos-library".into())
        .spawn(move || {
            let result = fetch_games(&host, mgmt_port, &identity, fingerprint);
            let _ = tx.send(GamesLoaded {
                host,
                port,
                mgmt_port,
                result,
            });
        })
        .expect("spawn library-fetch thread");
    rx
}

/// Fetches one piece of cover art's raw bytes (JPEG/PNG, undecoded) from a
/// host-relative `art_path` (one of `GameEntry::art`'s fields) — same mTLS pinning
/// as `fetch_games`, but takes an already-built `agent` so a caller fetching many
/// art paths in a row (see `art.rs`) reuses one connection instead of a fresh mTLS
/// handshake per call. Decoding happens in `art.rs`, off this module's REST concern.
pub fn fetch_art(agent: &ureq::Agent, addr: &str, mgmt_port: u16, art_path: &str) -> Result<Vec<u8>, LibraryError> {
    let url = format!("{}{art_path}", base_url(addr, mgmt_port));
    match agent.get(url.as_str()).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| LibraryError::Unreachable(format!("read art body: {e}"))),
        Err(e) => Err(classify(e)),
    }
}

fn classify(e: ureq::Error) -> LibraryError {
    match e {
        ureq::Error::StatusCode(401 | 403) => LibraryError::NotPaired,
        ureq::Error::StatusCode(code) => LibraryError::Http(code),
        // The one rejection our own `PinVerify` (below) actually raises on a mismatch —
        // matched on the typed `rustls::Error` ureq 3.x's `Error::Rustls` now carries,
        // instead of the string-matching `Transport(t)` message-sniffing ureq 2.x forced.
        ureq::Error::Rustls(rustls::Error::InvalidCertificate(
            rustls::CertificateError::ApplicationVerificationFailure,
        )) => LibraryError::PinMismatch,
        other => LibraryError::Unreachable(other.to_string()),
    }
}

/// Wraps a chained (TCP) transport in TLS using a caller-supplied `rustls::ClientConfig`
/// verbatim — modeled directly on ureq 3.x's own (crate-private) `RustlsConnector`
/// (`ureq` crate, `src/tls/rustls.rs`), minus its `TlsConfig`-driven `build_config` step,
/// since that step has no way to install `PinVerify`'s fingerprint-pinning verifier.
struct PinnedTlsConnector {
    config: Arc<rustls::ClientConfig>,
}

impl std::fmt::Debug for PinnedTlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTlsConnector").finish()
    }
}

impl<In: Transport> Connector<In> for PinnedTlsConnector {
    type Out = Either<In, PinnedTlsTransport>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            panic!("PinnedTlsConnector requires a chained transport");
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }

        let name: rustls::pki_types::ServerName<'_> = details
            .uri
            .authority()
            .expect("uri authority for tls")
            .host()
            .try_into()
            .map_err(|_| ureq::Error::Tls("invalid DNS name"))?;
        let conn = rustls::ClientConnection::new(self.config.clone(), name.to_owned())?;
        let stream = rustls::StreamOwned {
            conn,
            sock: TransportAdapter::new(transport.boxed()),
        };
        let buffers = LazyBuffers::new(details.config.input_buffer_size(), details.config.output_buffer_size());
        Ok(Some(Either::B(PinnedTlsTransport { buffers, stream })))
    }
}

struct PinnedTlsTransport {
    buffers: LazyBuffers,
    stream: rustls::StreamOwned<rustls::ClientConnection, TransportAdapter>,
}

impl std::fmt::Debug for PinnedTlsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTlsTransport").finish()
    }
}

impl Transport for PinnedTlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
