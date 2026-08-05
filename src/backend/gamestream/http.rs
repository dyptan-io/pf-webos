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
use moonlight_common::http::client::{RequestError, DEFAULT_LONG_TIMEOUT, DEFAULT_TIMEOUT};
use moonlight_common::http::{ClientInfo, Endpoint, ParseError, QueryBuilderError, Request as _, TextResponse};
use pem::Pem;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{Connector as _, TcpConnector};

use crate::services::pinned_tls::PinnedTlsConnector;

/// How long to wait for the TCP connect itself, independent of the global request timeout —
/// an unreachable host on a quiet LAN otherwise burns the whole 10 s (or 90 s) budget before
/// saying so.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(timeout))
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
        let body = self.agent.get(&url).call()?.body_mut().read_to_string()?;
        Ok(E::Response::from_str(&body)?)
    }
}

impl RequestClient for GsHttpClient {
    type Error = GsHttpError;

    fn with_defaults() -> Result<Self, Self::Error> {
        Ok(Self::plain(DEFAULT_TIMEOUT))
    }

    /// Used for pairing, where each phase waits on a human typing a PIN into the host's web
    /// UI — hence the crate's 90 s budget rather than the 10 s one.
    fn with_defaults_long_timeout() -> Result<Self, Self::Error> {
        Ok(Self::plain(DEFAULT_LONG_TIMEOUT))
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
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| bad("tls config", &e))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(ExactCertVerify {
                expected: expected.into_owned(),
            }))
            .with_client_auth_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    client_certificate.contents().to_vec(),
                )],
                client_key_der(client_private_key)?,
            )
            .map_err(|e| bad("client auth", &e))?;

        let connector = TcpConnector::default().chain(PinnedTlsConnector::new(Arc::new(cfg)));
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(DEFAULT_TIMEOUT))
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
        Ok(self.agent.get(&url).call()?.body_mut().read_to_vec()?)
    }
}

/// A query string that percent-encodes its values.
///
/// **Not just a nicety — pairing does not work without it.** `moonlight-common` builds queries
/// through the [`QueryBuilder`] trait, and its own `impl QueryBuilder for String` concatenates
/// values verbatim (with a `TODO: filter for characters that need % serialization` where the
/// encoding should be). Our device name is `webOS TV`, so `devicename=webOS TV` reached ureq, whose
/// `http::Uri` parse rejected the space: `InvalidUriChar`, on pairing phase 1, before a single byte
/// went to the host. Every other client encodes here too (moonlight-qt goes through `QUrlQuery`),
/// and Sunshine URL-decodes, so this is the interoperable behaviour rather than a workaround.
///
/// Keys are appended as-is: they are all `&'static str` literals in the crate's endpoints.
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

#[cfg(test)]
mod tests {
    use moonlight_common::http::{QueryBuilder as _, QueryParam};

    use super::EncodedQuery;

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
