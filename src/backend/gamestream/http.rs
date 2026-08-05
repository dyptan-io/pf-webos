//! `moonlight-common`'s `RequestClient` driven by this app's own `ureq` + `rustls` stack.
//!
//! The crate ships a `ureq` implementation, but it sits behind a feature that also drags
//! `hyper` in (its URL builder returns a `hyper::Uri`), and it disables certificate
//! verification outright — a `TODO: THIS MUST BE CHANGED` in the upstream source. Neither
//! is acceptable here, so this is our own implementation over the one HTTP stack the app
//! already has, with the server certificate pinned for real.
//!
//! **What "pinned" means here.** `GameStream` hosts serve a self-signed certificate with a
//! meaningless subject and, on Sunshine, a validity window that routinely doesn't match the
//! TV's clock. There is nothing to verify a name or a chain against — the only trust anchor
//! is the exact certificate the host handed us during pairing, so the verifier compares DER
//! bytes and nothing else. Handshake signatures are still checked properly, which is what
//! stops an active attacker from replaying that (public) certificate with its own key.
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use moonlight_common::http::client::blocking_client::RequestClient;
use moonlight_common::http::client::RequestError;
use moonlight_common::http::{ClientInfo, Endpoint, ParseError, QueryBuilderError, Request as _, TextResponse};
use pem::Pem;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{Connector as _, TcpConnector};

use crate::services::pinned_tls::PinnedTlsConnector;

// Every timeout below comes from `services::budget`, unaliased, so this file can't drift from the
// punktfunk side: `HANDSHAKE` for the TCP connect (an unreachable host on a quiet LAN would
// otherwise burn the whole request budget before saying so), `REQUEST` for a call that should
// already have an answer, and `HOST_WAIT` in place of the crate's own 90 s pairing budget.
use crate::services::budget;

/// How many times a request is re-sent after a failure that never reached the host's handler —
/// see [`GsHttpClient::get_with_retry`].
const HANDSHAKE_RETRIES: u32 = 2;
const RETRY_DELAY: Duration = Duration::from_millis(400);

// Pooling is off (`max_idle_connections(0)`) on both agents below: a pooled socket the host has
// already closed fails the next request as an immediate `UnexpectedEof` with nothing in the
// host's log — this is what broke pairing, whose PIN pause is exactly when a pooled connection
// goes stale. One extra TCP/TLS setup per request is cheap on these one-shot LAN calls.

#[derive(Debug)]
pub enum GsHttpError {
    Ureq(ureq::Error),
    Parse(ParseError),
    Tls(String),
}

impl std::fmt::Display for GsHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ureq(e) => write!(f, "http: {e}"),
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::Tls(e) => write!(f, "tls: {e}"),
        }
    }
}

impl std::error::Error for GsHttpError {}

impl GsHttpError {
    /// The host's own `<status_message>`, when it rejected a call and said why.
    ///
    /// This is the only part of a failed call worth putting on screen: it is the host's own wording
    /// ("Invalid PIN", "Already paired"), whereas everything wrapped around it names Rust types.
    fn host_message(&self) -> Option<&str> {
        match self {
            Self::Parse(ParseError::InvalidXmlStatusCode { message }) => message.as_deref(),
            _ => None,
        }
    }

    /// One plain sentence for a call that never got an answer.
    fn transport_message(&self) -> &'static str {
        match self {
            Self::Ureq(ureq::Error::Tls(_) | ureq::Error::Rustls(_)) => {
                "The host didn't accept this device — pair with it again."
            }
            Self::Tls(_) => "This device's pairing certificate couldn't be used.",
            Self::Ureq(_) => "The host couldn't be reached.",
            Self::Parse(_) => "The host's answer wasn't understood.",
        }
    }
}

/// One plain sentence for a failed host call, with the technical form logged rather than shown.
///
/// `moonlight-common`'s own `Display` is not showable: `MoonlightClientError::Backend` formats its
/// inner error with `{:?}`, so a host rejecting a pairing reached the screen as
/// `pair: pairing: request: Parse(InvalidXmlStatusCode { message: Some("Invalid PIN") })`. The
/// host's `message` inside that is the whole of what a user can act on, so it is pulled out and
/// everything else goes to the log.
pub fn api_message(what: &str, err: &moonlight_common::high::MoonlightClientError) -> String {
    use moonlight_common::high::MoonlightClientError as E;
    use moonlight_common::http::pair::client::ClientPairingError as P;

    // `{err:?}` rather than `{err}`: Debug is where the boxed inner errors survive intact, and this
    // line is the only remaining record of them.
    tracing::warn!("GameStream {what} failed: {err:?}");

    // A boxed backend error is our own `GsHttpError`, so the host's message is reachable.
    let backend = match err {
        E::Backend(inner) | E::Pairing(P::Crypto(inner)) => inner.downcast_ref::<GsHttpError>(),
        _ => None,
    };
    if let Some(inner) = backend {
        return inner
            .host_message()
            .map_or_else(|| inner.transport_message().to_string(), str::to_string);
    }
    match err {
        E::NotPaired => "This host hasn't been paired yet.".into(),
        E::Offline => "The host isn't responding.".into(),
        E::Unauthenticated => "The host didn't accept this device — pair with it again.".into(),
        E::Poisoned(_) => "Something went wrong talking to the host.".into(),
        // The crate's wording here is a developer's ("failed to pair because the pin was
        // incorrect"), and these three are the ones a user actually hits.
        E::Pairing(P::FailedWrongPin) => "The host says that PIN was wrong.".into(),
        E::Pairing(P::FailedAlreadyInProgress) => {
            "Another device is pairing with this host — try again shortly.".into()
        }
        E::Pairing(P::Failed) => "The host refused the pairing.".into(),
        // `StreamConfigError` names an unsupported setting, which is worth passing through: it is
        // the only thing that says *which* request the host can't meet.
        E::StreamConfig(_) | E::Moonlight(_) => format!("{err}"),
        E::Pairing(P::Crypto(_)) | E::Backend(_) => "The host couldn't be reached.".into(),
    }
}

impl From<ureq::Error> for GsHttpError {
    fn from(e: ureq::Error) -> Self {
        Self::Ureq(e)
    }
}

impl From<ParseError> for GsHttpError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}

impl RequestError for GsHttpError {
    fn is_connect(&self) -> bool {
        matches!(
            self,
            Self::Ureq(ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Io(_))
        )
    }

    /// The host no longer accepts our client certificate — on `GameStream` that means it
    /// dropped this device from its paired list, not that the transport is broken.
    fn is_encryption(&self) -> bool {
        matches!(self, Self::Ureq(ureq::Error::Tls(_) | ureq::Error::Rustls(_)))
    }
}

impl TryInto<ParseError> for GsHttpError {
    type Error = Self;

    fn try_into(self) -> Result<ParseError, Self::Error> {
        match self {
            Self::Parse(err) => Ok(err),
            other => Err(other),
        }
    }
}

/// A `RequestClient` that is either anonymous (HTTP only, pre-pairing) or holds the mTLS
/// identity for the authenticated HTTPS endpoints.
///
/// Cloneable because the trait demands it and `MoonlightHost` clones the client to make a
/// request without holding its mutex; the `Arc` keeps that from re-doing TLS setup.
#[derive(Clone)]
pub struct GsHttpClient {
    agent: Arc<ureq::Agent>,
}

impl GsHttpClient {
    fn plain(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(budget::HANDSHAKE))
            .timeout_global(Some(timeout))
            .max_idle_connections(0)
            .build();
        Self {
            agent: Arc::new(ureq::Agent::new_with_config(config)),
        }
    }

    /// `<scheme>://<hostport><path>?<query>`. `hostport` arrives from `MoonlightHost` already
    /// formatted; IPv6 literals need the brackets a bare `format!` wouldn't add.
    fn url<E: Endpoint>(use_https: bool, client_info: &ClientInfo, hostport: &str, request: &E::Request) -> String {
        let mut query = EncodedQuery::default();
        // `EncodedQuery`'s `append` is infallible, so neither of these can fail.
        let _ = client_info.append_query_params(&mut query);
        let _ = request.append_query_params(&mut query);
        let scheme = if use_https { "https" } else { "http" };
        format!("{scheme}://{hostport}{}?{}", E::path(), query.0)
    }

    fn text<E>(
        &self,
        use_https: bool,
        info: ClientInfo,
        hostport: &str,
        req: &E::Request,
    ) -> Result<E::Response, GsHttpError>
    where
        E: Endpoint,
        E::Response: TextResponse<Err = ParseError>,
    {
        let url = Self::url::<E>(use_https, &info, hostport, req);
        // The query, not just the path: every pairing phase is `/pair`, and *which* phase failed is
        // the whole diagnosis (`phrase=getservercert` / `clientchallenge` / `pairchallenge`, and
        // whether it went over HTTP or HTTPS). Debug-level, and the values here are the pairing
        // salt/challenge — public by design, since the PIN is what makes them secret.
        tracing::debug!("gamestream request: {url}");
        let body = self.get_with_retry(&url)?.body_mut().read_to_string()?;
        Ok(E::Response::from_str(&body)?)
    }

    /// A GET that survives a transient connect or handshake failure. Worth it because pairing's
    /// last phase is HTTPS: dying there leaves the host thinking it paired while the user has to
    /// fetch a fresh PIN. Safe for every endpoint — an unfinished handshake was never served.
    fn get_with_retry(&self, url: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let mut attempt = 0;
        loop {
            match self.agent.get(url).call() {
                Err(e) if attempt < HANDSHAKE_RETRIES && is_transport(&e) => {
                    attempt += 1;
                    tracing::warn!("gamestream request failed at the transport ({e}); retry {attempt}");
                    std::thread::sleep(RETRY_DELAY);
                }
                other => return other,
            }
        }
    }
}

/// Errors worth re-sending: a connection that was established and then broke before serving
/// anything, which is what a stale pooled socket or a mid-handshake drop looks like.
///
/// `ConnectionFailed` is deliberately *not* here, and neither is a connect timeout. Nothing was
/// listening — the host is off or unreachable, no retry changes that, and retrying tripled how long
/// a dead host took to fail (3 × [`budget::HANDSHAKE`] plus delays) where the punktfunk side spends
/// one handshake and reports it. TLS errors are out for the same reason: the one that happens is
/// the host rejecting our client certificate because it dropped this device, which is the
/// `is_encryption` signal.
fn is_transport(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Io(_))
}

impl RequestClient for GsHttpClient {
    type Error = GsHttpError;

    fn with_defaults() -> Result<Self, Self::Error> {
        Ok(Self::plain(budget::REQUEST))
    }

    /// Used for pairing, where each phase waits on a human typing a PIN into the host's web
    /// UI — hence [`budget::HOST_WAIT`] rather than the 10 s one.
    fn with_defaults_long_timeout() -> Result<Self, Self::Error> {
        Ok(Self::plain(budget::HOST_WAIT))
    }

    fn with_certificates(
        client_private_key: &Pem,
        client_certificate: &Pem,
        server_certificate: &Pem,
    ) -> Result<Self, Self::Error> {
        let bad = |what: &str, e: &dyn std::fmt::Display| GsHttpError::Tls(format!("{what}: {e}"));
        // Ring provider, matching `services::library` and punktfunk-core's QUIC.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let expected = rustls::pki_types::CertificateDer::from(server_certificate.contents().to_vec());
        let mut cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| bad("tls config", &e))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ExactCertVerify {
                expected: expected.into_owned(),
                provider,
            }))
            .with_client_auth_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    client_certificate.contents().to_vec(),
                )],
                client_key_der(client_private_key)?,
            )
            .map_err(|e| bad("client auth", &e))?;
        // Sunshine cannot resume a session it issued a ticket for: the resuming handshake dies on a
        // fatal `internal_error` alert. Since nothing here reuses a connection either (see the
        // pooling note above), every request after the first tried to resume and failed — which is
        // what broke pairing, whose last phase is the second HTTPS call in the handshake.
        cfg.resumption = rustls::client::Resumption::disabled();

        let connector = TcpConnector::default().chain(PinnedTlsConnector::new(Arc::new(cfg)));
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(budget::HANDSHAKE))
            .timeout_global(Some(budget::REQUEST))
            // See the pooling note at the top of this file: no pooling.
            .max_idle_connections(0)
            .build();
        Ok(Self {
            agent: Arc::new(ureq::Agent::with_parts(config, connector, DefaultResolver::default())),
        })
    }

    fn send_http<E>(&self, info: ClientInfo, hostport: &str, req: &E::Request) -> Result<E::Response, Self::Error>
    where
        E: Endpoint,
        E::Response: TextResponse<Err = ParseError>,
    {
        self.text::<E>(false, info, hostport, req)
    }

    fn send_https<E>(&self, info: ClientInfo, hostport: &str, req: &E::Request) -> Result<E::Response, Self::Error>
    where
        E: Endpoint,
        E::Response: TextResponse<Err = ParseError>,
    {
        self.text::<E>(true, info, hostport, req)
    }

    fn send_https_with_bytes<E>(
        &self,
        info: ClientInfo,
        hostport: &str,
        req: &E::Request,
    ) -> Result<E::Response, Self::Error>
    where
        E: Endpoint<Response = Vec<u8>>,
    {
        let url = Self::url::<E>(true, &info, hostport, req);
        tracing::debug!("gamestream request (binary): {} {}", E::path(), hostport);
        Ok(self.get_with_retry(&url)?.body_mut().read_to_vec()?)
    }
}

/// A query string that percent-encodes its values.
///
/// **Required, not cosmetic.** `moonlight-common`'s own `impl QueryBuilder for String` concatenates
/// values verbatim (its `TODO: filter for characters that need % serialization` is unimplemented),
/// so our `devicename=webOS TV` reached ureq's `http::Uri` parser as an unencoded space and failed
/// with `InvalidUriChar` before pairing sent a byte. moonlight-qt encodes too (via `QUrlQuery`) and
/// Sunshine URL-decodes, so this matches the interoperable behaviour.
#[derive(Default)]
struct EncodedQuery(String);

impl moonlight_common::http::QueryBuilder for EncodedQuery {
    fn append(&mut self, param: moonlight_common::http::QueryParam) -> Result<(), QueryBuilderError> {
        if !self.0.is_empty() {
            self.0.push('&');
        }
        self.0.push_str(param.key);
        self.0.push('=');
        for byte in param.value.bytes() {
            // RFC 3986 unreserved. Everything else is escaped, including `+`, which a decoder is
            // free to read as a space.
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                self.0.push(char::from(byte));
            } else {
                use std::fmt::Write as _;
                // Writing to a `String` cannot fail.
                let _ = write!(self.0, "%{byte:02X}");
            }
        }
        Ok(())
    }
}

/// The PEM tag decides the key encoding: `moonlight-common`'s own generator emits PKCS#8
/// (`PRIVATE KEY`), but an identity written by another Moonlight client — which is a
/// supported thing to drop in, since the host keys pairing to the certificate — is usually
/// PKCS#1 (`RSA PRIVATE KEY`).
fn client_key_der(key: &Pem) -> Result<rustls::pki_types::PrivateKeyDer<'static>, GsHttpError> {
    let der = key.contents().to_vec();
    match key.tag() {
        "PRIVATE KEY" => Ok(rustls::pki_types::PrivatePkcs8KeyDer::from(der).into()),
        "RSA PRIVATE KEY" => Ok(rustls::pki_types::PrivatePkcs1KeyDer::from(der).into()),
        other => Err(GsHttpError::Tls(format!("unsupported private key PEM tag {other:?}"))),
    }
}

/// Trusts exactly one certificate, by DER equality — see this module's docs for why there is
/// nothing else to check against on a `GameStream` host.
#[derive(Debug)]
struct ExactCertVerify {
    expected: rustls::pki_types::CertificateDer<'static>,
    /// The same provider the `ClientConfig` was built with — held so the signature-verification
    /// hooks below don't construct one per call (they run 2-4 times per request).
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for ExactCertVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected.as_ref() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use moonlight_common::http::{QueryBuilder as _, QueryParam};

    use super::{api_message, EncodedQuery, GsHttpError};
    use moonlight_common::high::MoonlightClientError;
    use moonlight_common::http::ParseError;

    /// The whole point of `api_message`: the host's own words reach the user, and the Rust type
    /// names around them do not.
    #[test]
    fn host_status_message_is_what_surfaces() {
        let rejected = GsHttpError::Parse(ParseError::InvalidXmlStatusCode {
            message: Some("Invalid PIN".to_string()),
        });
        let err = MoonlightClientError::Backend(Box::new(rejected));
        assert_eq!(api_message("pairing", &err), "Invalid PIN");
    }

    /// A host that rejected without saying why must still not leak the enum into the status line.
    #[test]
    fn a_message_less_rejection_falls_back_to_a_sentence() {
        let rejected = GsHttpError::Parse(ParseError::InvalidXmlStatusCode { message: None });
        let err = MoonlightClientError::Backend(Box::new(rejected));
        assert_eq!(api_message("pairing", &err), "The host's answer wasn't understood.");
    }

    /// The bug this type exists for: an unencoded space in `devicename` made ureq's URI parse
    /// reject every pairing request with `InvalidUriChar`.
    #[test]
    fn values_are_percent_encoded() {
        let mut q = EncodedQuery::default();
        q.append(QueryParam {
            key: "devicename",
            value: "webOS TV",
        })
        .unwrap();
        q.append(QueryParam {
            key: "salt",
            value: "0a1b",
        })
        .unwrap();
        assert_eq!(q.0, "devicename=webOS%20TV&salt=0a1b");
    }
}
