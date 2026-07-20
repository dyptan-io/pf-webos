//! LAN host discovery via mDNS — mirrors `pf-client-core::discovery`'s shape
//! (`_punktfunk._udp` advert, same TXT keys) but as our own direct `mdns-sd`
//! dependency rather than depending on `pf-client-core` itself (see `session.rs`
//! docs for why: its Cargo.toml would drag in FFmpeg/PipeWire for our target too).
use std::io::Write as _;

use mdns_sd::{ServiceDaemon, ServiceEvent};

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// The management API's port, from the mDNS `mgmt` TXT — `None` if the host
    /// doesn't advertise one (falls back to `library::DEFAULT_MGMT_PORT`).
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MAC(s) (`aa:bb:cc:dd:ee:ff`) from the mDNS `mac` TXT
    /// (comma-separated) — learned while the host is awake and advertising, and
    /// persisted onto the matching known host so it can be woken later once it
    /// goes offline (see `app::App::drain_discovery`). Empty if not advertised.
    pub mac: Vec<String>,
}

/// Browse continuously. The spawned thread does NOT reliably exit just because the
/// returned `Receiver<DiscoveredHost>` is dropped: only a `ServiceEvent::ServiceResolved`
/// checks `tx.send(..).is_err()` to notice that and stop — every other event kind
/// (in practice, an endless stream of `SearchStarted`) loops forever regardless,
/// burning a thread's worth of CPU/network activity for the rest of the process's
/// life. Confirmed live: `mdns: SearchStarted(...)` kept appearing throughout active
/// game-streaming sessions, well after the menu (and its `App`) was long gone. The
/// returned `ServiceDaemon` handle is `Clone` and lets a caller call `shutdown()`
/// explicitly once discovery is no longer needed (see `App`'s `Drop` impl) — that
/// unblocks the thread's `receiver.recv()` promptly instead of waiting on a lucky
/// future resolution event. `log` is a second handle onto the app's own log file
/// (see `main.rs::log_path`) — every failure/event point here is logged, since this
/// previously failed completely silently: a `ServiceDaemon::new()`/`browse()` error,
/// or every non-`ServiceResolved` event, was just dropped with no trace, making "no
/// hosts showed up" undiagnosable from the log alone (was it a permissions/socket
/// failure, wrong interface, or genuinely nothing advertising?).
pub fn browse(mut log: std::fs::File) -> Option<(std::sync::mpsc::Receiver<DiscoveredHost>, ServiceDaemon)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let daemon = ServiceDaemon::new()
        .inspect_err(|e| {
            let _ = writeln!(log, "mdns: ServiceDaemon::new failed: {e}");
        })
        .ok()?;
    let daemon_handle = daemon.clone();
    std::thread::Builder::new()
        .name("punktfunk-webos-mdns".into())
        .spawn(move || {
            let receiver = match daemon.browse("_punktfunk._udp.local.") {
                Ok(r) => r,
                Err(e) => {
                    let _ = writeln!(log, "mdns: browse(_punktfunk._udp.local.) failed: {e}");
                    return;
                }
            };
            let _ = writeln!(log, "mdns: browsing _punktfunk._udp.local.");
            while let Ok(event) = receiver.recv() {
                let info = match event {
                    ServiceEvent::ServiceResolved(info) => info,
                    other => {
                        let _ = writeln!(log, "mdns: {other:?}");
                        continue;
                    }
                };
                // IPv4 only, same policy as the other clients — the core dials
                // `format!("{host}:{port}").parse::<SocketAddr>()` over IPv4.
                let Some(addr) = info
                    .get_addresses_v4()
                    .iter()
                    .next()
                    .map(std::string::ToString::to_string)
                else {
                    let _ = writeln!(
                        log,
                        "mdns: resolved {} with no IPv4 address, skipping",
                        info.get_fullname()
                    );
                    continue;
                };
                let props = info.get_properties();
                let host = DiscoveredHost {
                    name: info.get_fullname().split('.').next().unwrap_or("?").to_string(),
                    addr,
                    port: info.get_port(),
                    mgmt_port: props.get_property_val_str("mgmt").and_then(|v| v.parse().ok()),
                    mac: props
                        .get_property_val_str("mac")
                        .unwrap_or("")
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                };
                let _ = writeln!(log, "mdns: resolved {} at {}:{}", host.name, host.addr, host.port);
                if tx.send(host).is_err() {
                    break; // receiver gone — stop browsing
                }
            }
            let _ = writeln!(log, "mdns: receiver loop ended, shutting down");
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    Some((rx, daemon_handle))
}
