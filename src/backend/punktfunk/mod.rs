//! The punktfunk backend. Behaviour-neutral by construction: every method delegates to the
//! `services`/`session` code that already implements it, so introducing the seam cannot change
//! how a punktfunk host behaves.
use mdns_sd::ResolvedService;

use crate::backend::{ArtFetch, BackendCaps, HostBackend};
use crate::core::model::GameEntry;
use crate::core::protocol::Protocol;
use crate::services::discovery::{self, DiscoveredHost};
use crate::services::library::{self, LibraryError};

/// mDNS service type punktfunk hosts advertise.
pub const SERVICE_TYPE: &str = "_punktfunk._udp.local.";

pub struct Punktfunk;

impl HostBackend for Punktfunk {
    fn protocol(&self) -> Protocol {
        Protocol::Punktfunk
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps {
            speed_test: true,
            request_access: true,
            unpair: false,
            // A punktfunk session lives for as long as the connection does, so there is never a
            // leftover app on the host to end.
            quit_app: false,
        }
    }

    fn default_query_port(&self) -> u16 {
        library::DEFAULT_MGMT_PORT
    }

    fn probe(&self, addr: &str, port: u16, timeout: std::time::Duration) -> bool {
        punktfunk_core::client::NativeClient::probe(addr, port, timeout)
    }

    fn discovery_service(&self) -> &'static str {
        SERVICE_TYPE
    }

    fn parse_discovery(&self, info: &ResolvedService) -> Option<DiscoveredHost> {
        let (addr, name) = discovery::addr_and_name(info)?;
        let props = info.get_properties();
        Some(DiscoveredHost {
            name,
            addr,
            port: info.get_port(),
            protocol: self.protocol(),
            mgmt_port: props.get_property_val_str("mgmt").and_then(|v| v.parse().ok()),
            mac: props
                .get_property_val_str("mac")
                .unwrap_or("")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }

    fn list_games(
        &self,
        addr: &str,
        query_port: u16,
        identity: &(String, String),
        pin: Option<[u8; 32]>,
    ) -> Result<Vec<GameEntry>, LibraryError> {
        library::fetch_games(addr, query_port, identity, pin)
    }

    fn unpair(&self, _addr: &str, _query_port: u16) -> anyhow::Result<()> {
        // The host authorizes by client certificate against a pin we hold; dropping the record is
        // the whole of unpairing, and `App::forget_host` has already done it. Unreached in
        // practice (that call site checks `caps().unpair` first), but `Ok` rather than a panic:
        // "nothing left to do" is the true answer, not an unsupported operation.
        Ok(())
    }

    fn quit_app(&self, _addr: &str, _query_port: u16) -> anyhow::Result<bool> {
        // Disconnecting ends the session, so nothing outlives it to be quit. `Ok(false)` — "there
        // was nothing to end" — for the same reason `unpair` returns `Ok(())`: it is the true
        // answer, not an unsupported operation. Unreached; `caps().quit_app` is false.
        Ok(false)
    }

    fn art_fetcher(
        &self,
        addr: &str,
        query_port: u16,
        identity: &(String, String),
        pin: Option<[u8; 32]>,
    ) -> Result<Box<dyn ArtFetch>, LibraryError> {
        Ok(Box::new(MtlsArt {
            agent: library::agent(identity, pin)?,
            addr: addr.to_string(),
            mgmt_port: query_port,
        }))
    }
}

/// The mTLS art transport. One `ureq::Agent` for every cover in a library: a fresh one per
/// cover would mean a fresh TCP+TLS handshake including client-cert auth, real avoidable cost
/// that scales with library size.
struct MtlsArt {
    agent: ureq::Agent,
    addr: String,
    mgmt_port: u16,
}

impl ArtFetch for MtlsArt {
    fn fetch(&self, art: &str) -> Result<Vec<u8>, LibraryError> {
        library::fetch_art(&self.agent, &self.addr, self.mgmt_port, art)
    }
}
