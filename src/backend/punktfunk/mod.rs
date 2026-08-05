//! The punktfunk backend. Behaviour-neutral by construction: every method delegates to the
//! `services`/`session` code that already implements it, so introducing the seam cannot change
//! how a punktfunk host behaves.
use mdns_sd::ResolvedService;

use crate::backend::{BackendCaps, HostBackend};
use crate::core::model::GameEntry;
use crate::core::protocol::Protocol;
use crate::services::discovery::DiscoveredHost;
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
        }
    }

    fn discovery_service(&self) -> &'static str {
        SERVICE_TYPE
    }

    fn parse_discovery(&self, info: &ResolvedService) -> Option<DiscoveredHost> {
        // IPv4 only (same as other clients).
        let addr = info
            .get_addresses_v4()
            .iter()
            .next()
            .map(std::string::ToString::to_string);
        let Some(addr) = addr else {
            tracing::warn!("mdns: resolved {} with no IPv4 address, skipping", info.get_fullname());
            return None;
        };
        let props = info.get_properties();
        Some(DiscoveredHost {
            name: info.get_fullname().split('.').next().unwrap_or("?").to_string(),
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
        mgmt_port: u16,
        identity: &(String, String),
        pin: Option<[u8; 32]>,
    ) -> Result<Vec<GameEntry>, LibraryError> {
        library::fetch_games(addr, mgmt_port, identity, pin)
    }
}
