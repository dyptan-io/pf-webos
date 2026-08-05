//! `GameStream` (Sunshine and compatible forks) support.
//!
//! P3 of `docs/GameStream-Plan.md`: the non-streaming half behind [`HostBackend`]. The
//! protocol work is `moonlight-common`'s; this module supplies the HTTP stack ([`http`]), the
//! persisted identity ([`identity`]), the host calls ([`query`]) and the mapping between the
//! crate's types and ours (here).
//!
//! Nothing here is gated on `Settings::gamestream_enabled` — the gate sits at the points where
//! the *app* would start speaking `GameStream` (the mDNS browse in `App::new`, the manual-IP
//! fallback probe, sidebar filtering), because a host already paired while the toggle was on
//! must still be listable by this backend when something asks it directly.
use mdns_sd::ResolvedService;
use moonlight_common::AppId;

use crate::backend::{ArtFetch, BackendCaps, HostBackend};
use crate::core::model::{Artwork, GameEntry};
use crate::core::protocol::Protocol;
use crate::services::discovery::DiscoveredHost;
use crate::services::library::LibraryError;

pub mod http;
pub mod identity;
pub mod query;

/// mDNS service type `GameStream` hosts advertise. The SRV port on a record of this type is the
/// host's *HTTP* port; the HTTPS one is only knowable from `/serverinfo`.
pub const SERVICE_TYPE: &str = "_nvstream._tcp.local.";

/// The plain-HTTP query port, used when nothing advertised one.
pub const DEFAULT_HTTP_PORT: u16 = moonlight_common::http::DEFAULT_HTTP_PORT;

pub struct GameStream;

impl HostBackend for GameStream {
    fn protocol(&self) -> Protocol {
        Protocol::GameStream
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps {
            // No bandwidth-probe endpoint in the protocol.
            speed_test: false,
            // PIN-only, and the PIN travels the other way — see `App::open_pairing`.
            request_access: false,
            unpair: true,
        }
    }

    fn default_query_port(&self) -> u16 {
        DEFAULT_HTTP_PORT
    }

    fn discovery_service(&self) -> &'static str {
        SERVICE_TYPE
    }

    fn parse_discovery(&self, info: &ResolvedService) -> Option<DiscoveredHost> {
        let addr = info
            .get_addresses_v4()
            .iter()
            .next()
            .map(std::string::ToString::to_string);
        let Some(addr) = addr else {
            tracing::warn!("mdns: resolved {} with no IPv4 address, skipping", info.get_fullname());
            return None;
        };
        let port = info.get_port();
        Some(DiscoveredHost {
            name: info.get_fullname().split('.').next().unwrap_or("?").to_string(),
            addr,
            port,
            protocol: self.protocol(),
            // One port, not two: `GameStream` serves queries and launches from the same
            // HTTP/HTTPS pair, so the "management" port is the query port. Carried in both
            // fields so a host saved from discovery needs no protocol-specific defaulting
            // later.
            mgmt_port: Some(port),
            // A `GameStream` host advertises no MAC over mDNS. `/serverinfo` has one, but
            // reading it would mean a round trip per discovery event; Wake-on-LAN for these
            // hosts can wait for a reason to exist.
            mac: Vec::new(),
        })
    }

    fn list_games(
        &self,
        addr: &str,
        query_port: u16,
        _identity: &(String, String),
        _pin: Option<[u8; 32]>,
    ) -> Result<Vec<GameEntry>, LibraryError> {
        // Neither argument applies: the punktfunk mTLS identity is a different key type with a
        // different subject (see `identity`), and there is no host leaf to pin — trust is the
        // client certificate the host registered at pairing time.
        let host = open(addr, query_port)?;
        // `is_paired` reads the cached `/serverinfo` that `open` just fetched, so this
        // distinguishes "host says we aren't on its list" from a transport failure without a
        // second round trip. Without it every unpaired host would read as `Unreachable` and
        // open the Wake dialog.
        if !host.is_paired().unwrap_or(false) {
            return Err(LibraryError::NotPaired);
        }
        let apps = query::app_list(&host).map_err(|e| LibraryError::Unreachable(e.to_string()))?;
        Ok(apps
            .into_iter()
            .map(|app| GameEntry {
                // Decimal `<ID>` from `/applist`, opaque above this layer exactly as
                // punktfunk's `steam:<appid>` is — and it is also the `art` handle below.
                id: app.id.0.to_string(),
                title: app.title,
                art: Artwork {
                    // `/appasset` serves one box-art image per app and nothing else, so the
                    // portrait slot is the only one that can be filled. A `GameStream` game
                    // therefore never gets a hero backdrop on the connecting screen, which
                    // falls back to its plain fade.
                    portrait: Some(app.id.0.to_string()),
                    ..Artwork::default()
                },
            })
            .collect())
    }

    fn unpair(&self, addr: &str, query_port: u16) -> anyhow::Result<()> {
        query::unpair(&query::open(addr, Some(query_port))?)
    }

    fn art_fetcher(
        &self,
        addr: &str,
        query_port: u16,
        _identity: &(String, String),
        _pin: Option<[u8; 32]>,
    ) -> Result<Box<dyn ArtFetch>, LibraryError> {
        Ok(Box::new(GsArt {
            host: open(addr, query_port)?,
        }))
    }
}

/// [`query::open`] with its error mapped into the library taxonomy. `open` fetches
/// `/serverinfo`, so a failure here is a reachability failure.
fn open(addr: &str, query_port: u16) -> Result<query::Host, LibraryError> {
    query::open(addr, Some(query_port)).map_err(|e| LibraryError::Unreachable(e.to_string()))
}

/// Box-art fetcher over `/appasset`. Holds the host open for the life of one library's art
/// loading, so the HTTPS handshake is paid once rather than per cover — the same reason
/// punktfunk's fetcher holds a `ureq::Agent`.
struct GsArt {
    host: query::Host,
}

impl ArtFetch for GsArt {
    fn fetch(&self, art: &str) -> Result<Vec<u8>, LibraryError> {
        let id: u32 = art
            .parse()
            .map_err(|_| LibraryError::Unreachable(format!("bad GameStream app id {art:?}")))?;
        query::box_art(&self.host, AppId(id)).map_err(|e| LibraryError::Unreachable(e.to_string()))
    }
}
