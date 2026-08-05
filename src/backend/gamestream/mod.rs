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
use crate::services::discovery::{self, DiscoveredHost};
use crate::services::library::LibraryError;

pub mod http;
pub mod identity;
pub mod input;
pub mod query;
/// Gated like `platform`/`session` in `main.rs`: this one drives the video sink and NDL, so it is
/// the only part of the backend that can't typecheck on a non-webOS host.
#[cfg(target_os = "linux")]
pub mod stream;

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
            // The host keeps the session (and the game) running after this client disconnects,
            // which is also why a second connect resumes rather than relaunches.
            quit_app: true,
        }
    }

    fn default_query_port(&self) -> u16 {
        DEFAULT_HTTP_PORT
    }

    /// A TCP connect to the HTTP port, not a `/serverinfo` fetch: this runs every sweep for
    /// every row, and a listening port is all the dot claims. Sunshine binds it whenever it is
    /// running, so it also distinguishes "machine up, host software down" — which is exactly
    /// what the shadowing rule needs.
    fn probe(&self, addr: &str, port: u16, timeout: std::time::Duration) -> bool {
        let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(addr, port)) else {
            return false;
        };
        addrs
            .into_iter()
            .any(|a| std::net::TcpStream::connect_timeout(&a, timeout).is_ok())
    }

    fn discovery_service(&self) -> &'static str {
        SERVICE_TYPE
    }

    fn parse_discovery(&self, info: &ResolvedService) -> Option<DiscoveredHost> {
        let (addr, name) = discovery::addr_and_name(info)?;
        let port = info.get_port();
        Some(DiscoveredHost {
            name,
            addr,
            port,
            protocol: self.protocol(),
            // One port, not two: `GameStream` serves queries and launches from the same
            // HTTP/HTTPS pair, so the "management" port is the query port. Carried in both
            // fields so a host saved from discovery needs no protocol-specific defaulting
            // later.
            mgmt_port: Some(port),
            // A `GameStream` host advertises no MAC over mDNS. `/serverinfo` has one, but
            // reading it would mean a round trip per discovery event — so it is learned later,
            // once, via `wake_mac`.
            mac: Vec::new(),
        })
    }

    fn wake_mac(&self, addr: &str, query_port: u16) -> Vec<String> {
        let host = match open(addr, query_port) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!("GameStream host {addr}: MAC lookup could not open host ({e})");
                return Vec::new();
            }
        };
        reported_mac(addr, &host)
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

    fn quit_app(&self, addr: &str, query_port: u16) -> anyhow::Result<bool> {
        let mut host = query::open(addr, Some(query_port))?;
        // Idle hosts need no round trip to `/cancel`, and asking anyway would report "nothing was
        // ended" for a host that is merely someone else's — `current_game` separates the two.
        if query::current_game(&host)?.is_none() {
            return Ok(false);
        }
        query::quit_running_app(&mut host)
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

/// The Wake-on-LAN MAC an open host reports in `/serverinfo`, or none — a host that reports no MAC
/// and one whose answer didn't parse are the same "nothing to wake with" to every caller, so both
/// are logged here rather than handed back.
pub(crate) fn reported_mac(addr: &str, host: &query::Host) -> Vec<String> {
    match host.mac() {
        Ok(Some(mac)) => vec![mac.to_string()],
        Ok(None) => {
            tracing::info!("GameStream host {addr}: no MAC in /serverinfo");
            Vec::new()
        }
        Err(e) => {
            tracing::debug!("GameStream host {addr}: MAC unreadable ({e:?})");
            Vec::new()
        }
    }
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
