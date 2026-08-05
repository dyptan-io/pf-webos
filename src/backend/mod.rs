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
    // Two more caps the plan calls for arrive with the code that reads them, rather than sitting
    // here unread: `host_abr` (false ⇒ resolve "Automatic" bitrate client-side) in P4, and
    // `unpair` (a host-side endpoint, so an action distinct from Forget) in P3.
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

    /// Fetches the host's game library, blocking. `mgmt_port` is already defaulted by the
    /// caller, since the fallback port differs per protocol.
    ///
    /// The error type is still `LibraryError` rather than a neutral one because
    /// `App::handle_library_error` switches the Wake dialog on `Unreachable` specifically — the
    /// protocol-neutral taxonomy that replaces it is P5's `errors.rs` work.
    fn list_games(
        &self,
        addr: &str,
        mgmt_port: u16,
        identity: &(String, String),
        pin: Option<[u8; 32]>,
    ) -> Result<Vec<GameEntry>, LibraryError>;
}

/// The registry. Static dispatch table, so a `Protocol` is all any screen needs to carry.
///
/// `None` means "this build can't talk to that host": P3 adds the `GameStream` backend, and until
/// then the only way to hold a `GameStream` `KnownHost` is to hand-edit `known-hosts.json`.
/// Returning `None` rather than falling back to punktfunk keeps that a visible dead end instead
/// of a confusing wrong-protocol connection attempt.
pub fn backend_for(protocol: Protocol) -> Option<&'static dyn HostBackend> {
    match protocol {
        Protocol::Punktfunk => Some(&punktfunk::Punktfunk),
        Protocol::GameStream => None,
    }
}

/// For the call paths that have no way to *report* an unsupported host — a probe whose only
/// output is a channel, a render pass. Logs loudly and uses punktfunk, which is correct until P3
/// because nothing can create a `GameStream` host yet. One named, documented escape hatch beats an
/// ad-hoc `unwrap_or` at each such site.
pub fn backend_or_punktfunk(protocol: Protocol) -> &'static dyn HostBackend {
    backend_for(protocol).unwrap_or_else(|| {
        tracing::error!("no backend for {protocol:?}; falling back to punktfunk");
        &punktfunk::Punktfunk
    })
}

/// Every backend this build can browse for. `services::discovery` iterates these rather than
/// hardcoding a service type, so P3's second browse is a one-line addition here.
pub const ALL_BACKENDS: &[&dyn HostBackend] = &[&punktfunk::Punktfunk];
