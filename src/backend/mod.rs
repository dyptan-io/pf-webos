//! The protocol seam. One trait per streaming protocol, so nothing above this layer branches on
//! [`Protocol`] — screens hold a `Protocol` (from `KnownHost`) and ask [`backend_for`] for the
//! behaviour. See `docs/GameStream-Plan.md`.
//!
//! Every method here is **blocking**. The off-thread spawning stays where it already is
//! (`services::library::load_games_async`, `app::state::pairing`), because those own the channel
//! the UI tick drains — a backend that spawned its own threads would duplicate that machinery.
//!
//! Depends on `core` and `services` only; it must not know about `app`, `ui` or `platform`.
use crate::core::model::GameEntry;
use crate::core::protocol::Protocol;
use crate::services::discovery::DiscoveredHost;
use crate::services::library::LibraryError;

pub mod gamestream;
pub mod punktfunk;

/// Which optional host features a protocol actually has. Screens use this to *omit* affordances,
/// never to show one that fails when pressed — see `app::state::hostmenu`. Adding a protocol
/// that lacks a feature must therefore touch no view code.
#[derive(Clone, Copy, Debug)]
pub struct BackendCaps {
    /// The pre-connect bandwidth probe (`session::run_speed_probe`). punktfunk only.
    pub speed_test: bool,
    /// Trust-on-first-use pairing with no PIN — punktfunk's "request access" park-and-approve.
    /// `GameStream` is PIN-only, and its PIN travels the other way (we display, the user types).
    pub request_access: bool,
    /// The host stores the pairing too, so dropping it needs a call to the host — an action
    /// distinct from Forget, which only discards our own record. punktfunk has no such endpoint:
    /// forgetting the pin *is* unpairing.
    pub unpair: bool,
    /// The host keeps running the app after this client leaves, so ending it is a separate
    /// action ([`HostBackend::quit_app`]). A punktfunk host's session *is* the connection —
    /// there is nothing left behind to quit.
    pub quit_app: bool,
    // `host_abr` (false ⇒ resolve "Automatic" bitrate client-side) is read inside
    // `backend::gamestream::stream` rather than through this struct: the decision is made where
    // `/launch` is built, and no screen asks.
}

/// One protocol's host behaviour.
pub trait HostBackend: Send + Sync {
    fn protocol(&self) -> Protocol;
    fn caps(&self) -> BackendCaps;

    /// The mDNS service type this backend owns. The type alone determines the protocol, so a
    /// discovered host is never probed to find out what it speaks.
    fn discovery_service(&self) -> &'static str;

    /// Turns a resolved mDNS record into a host, or `None` if it isn't usable (no IPv4, etc).
    fn parse_discovery(&self, info: &mdns_sd::ResolvedService) -> Option<DiscoveredHost>;

    /// Port for library and art queries when neither mDNS nor a saved record supplied one —
    /// punktfunk's management API and `GameStream`'s HTTP endpoint are different services on
    /// different default ports, so the fallback can't live at the call site.
    fn default_query_port(&self) -> u16;

    /// Whether the host answers on `port` right now, blocking for at most `timeout`. Feeds the
    /// sidebar's online dot (`app::state::reach`) and the `GameStream` shadowing rule
    /// (`App::refresh_gamestream_shadowing`), so it has to be as cheap as a probe can be —
    /// nothing here fetches a library or completes a handshake beyond what liveness needs.
    fn probe(&self, addr: &str, port: u16, timeout: std::time::Duration) -> bool;

    /// Wake-on-LAN MAC(s) the host reports about itself, blocking; empty when it reports none.
    ///
    /// Only for protocols whose MAC does *not* arrive with discovery. punktfunk advertises it in
    /// its mDNS TXT, so the default is "nothing to learn" — the caller keeps the record it already
    /// has. Costs a real query, so callers must only ask for a host whose MAC is still unknown.
    fn wake_mac(&self, _addr: &str, _query_port: u16) -> Vec<String> {
        Vec::new()
    }

    /// Fetches the host's game library, blocking. `query_port` is already defaulted by the
    /// caller via [`Self::default_query_port`].
    ///
    /// The error type is still `LibraryError` rather than a neutral one because
    /// `App::handle_library_error` switches the Wake dialog on `Unreachable` specifically — the
    /// protocol-neutral taxonomy that replaces it is P5's `errors.rs` work.
    fn list_games(
        &self,
        addr: &str,
        query_port: u16,
        identity: &(String, String),
        pin: Option<[u8; 32]>,
    ) -> Result<Vec<GameEntry>, LibraryError>;

    /// Tells the host to forget this device, blocking. A no-op rather than an error where
    /// [`BackendCaps::unpair`] is clear: for punktfunk, discarding our pin *is* unpairing, so
    /// "nothing more to do" is the honest answer and Forget can call this unconditionally.
    fn unpair(&self, addr: &str, query_port: u16) -> anyhow::Result<()>;

    /// Ends the app the host is running, blocking. `false` = there was nothing to end (see
    /// `gamestream::query::quit_running_app`). Only called where [`BackendCaps::quit_app`] is set.
    fn quit_app(&self, addr: &str, query_port: u16) -> anyhow::Result<bool>;

    /// Opens a reusable cover-art fetcher for one host, blocking. Built lazily by
    /// `services::art`'s worker on the first cover that isn't already cached, so a fully
    /// cached library never opens a connection — which is also why the returned value carries
    /// no `Send` bound: it is created and dropped inside that one thread.
    fn art_fetcher(
        &self,
        addr: &str,
        query_port: u16,
        identity: &(String, String),
        pin: Option<[u8; 32]>,
    ) -> Result<Box<dyn ArtFetch>, LibraryError>;
}

/// One host's cover-art transport, held open across a library's worth of fetches.
///
/// `art` is whatever the backend put in [`GameEntry::art`] and is opaque to
/// `services::art`: a host-relative path or absolute URL for punktfunk, a decimal app id for
/// `GameStream`. Keeping it uninterpreted is what stops the art loader growing a protocol
/// branch.
pub trait ArtFetch {
    fn fetch(&self, art: &str) -> Result<Vec<u8>, LibraryError>;
}

/// The registry. Static dispatch table, so a `Protocol` is all any screen needs to carry.
///
/// Total since P3, which is why there is no fallback helper: every protocol a `KnownHost` can
/// name has a backend, so a caller never has to decide what to do without one.
pub fn backend_for(protocol: Protocol) -> &'static dyn HostBackend {
    match protocol {
        Protocol::Punktfunk => &punktfunk::Punktfunk,
        Protocol::GameStream => &gamestream::GameStream,
    }
}

/// Every backend this build knows. `services::discovery` iterates the enabled subset rather than
/// hardcoding a service type.
pub const ALL_BACKENDS: &[&dyn HostBackend] = &[&punktfunk::Punktfunk, &gamestream::GameStream];

/// The backends to browse for, given the `Settings::gamestream_enabled` toggle. With it off the
/// client behaves exactly as it did before `GameStream` existed: no `_nvstream._tcp` browse at
/// all, so a Sunshine host on the LAN is never even resolved.
pub fn browse_backends(gamestream_enabled: bool) -> Vec<&'static dyn HostBackend> {
    ALL_BACKENDS
        .iter()
        .copied()
        .filter(|b| gamestream_enabled || b.protocol() != Protocol::GameStream)
        .collect()
}
