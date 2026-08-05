//! LAN discovery via mDNS. Direct mdns-sd dep to avoid pf-client-core's FFmpeg/PipeWire.
//!
//! One daemon browses one service type per enabled backend, and the type a record arrived on is
//! what sets its protocol — hosts are never probed to find out what they speak.
use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::core::protocol::Protocol;

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Set from the mDNS service type this record arrived on, by the owning backend's
    /// `parse_discovery`.
    pub protocol: Protocol,
    /// Management API port from mDNS (None → `library::DEFAULT_MGMT_PORT`).
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MACs from mDNS (learned while awake, persisted to `KnownHost`).
    pub mac: Vec<String>,
}

/// IPv4 address and short instance name from a resolved record — the two fields every backend's
/// `parse_discovery` needs, extracted identically. IPv4 only (same as other clients).
pub fn addr_and_name(info: &mdns_sd::ResolvedService) -> Option<(String, String)> {
    let Some(addr) = info
        .get_addresses_v4()
        .iter()
        .next()
        .map(std::string::ToString::to_string)
    else {
        tracing::warn!("mdns: resolved {} with no IPv4 address, skipping", info.get_fullname());
        return None;
    };
    Some((addr, info.get_fullname().split('.').next().unwrap_or("?").to_string()))
}

/// Browse continuously for `backends` (see `backend::browse_backends`); returns (Receiver,
/// `ServiceDaemon`). Thread won't exit until `ServiceDaemon::shutdown()` called explicitly —
/// mdns-sd's `SearchStarted` events loop forever. Failure points logged via tracing.
pub fn browse(
    backends: Vec<&'static dyn crate::backend::HostBackend>,
) -> Option<(std::sync::mpsc::Receiver<DiscoveredHost>, ServiceDaemon)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let daemon = ServiceDaemon::new()
        .inspect_err(|e| {
            tracing::error!("mdns: ServiceDaemon::new failed: {e}");
        })
        .ok()?;
    let daemon_handle = daemon.clone();
    std::thread::Builder::new()
        .name("punktfunk-webos-mdns".into())
        .spawn(move || {
            // One thread per service type, each blocking on its own receiver and forwarding into
            // the shared channel: a blocking `recv` costs nothing while idle, where one loop
            // polling several receivers would have to wake up forever. Each remembers which
            // backend owns it so a resolved record is parsed by the protocol that advertised it.
            let mut workers = Vec::new();
            for backend in backends {
                let service = backend.discovery_service();
                let receiver = match daemon.browse(service) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("mdns: browse({service}) failed: {e}");
                        continue;
                    }
                };
                tracing::debug!("mdns: browsing {service}");
                let tx = tx.clone();
                let worker = std::thread::Builder::new()
                    .name("punktfunk-webos-mdns-browse".into())
                    .spawn(move || {
                        while let Ok(event) = receiver.recv() {
                            let info = match event {
                                ServiceEvent::ServiceResolved(info) => info,
                                other => {
                                    tracing::debug!("mdns: {other:?}");
                                    continue;
                                }
                            };
                            let Some(host) = backend.parse_discovery(&info) else {
                                continue;
                            };
                            tracing::info!(
                                "mdns: resolved {} at {}:{} ({:?})",
                                host.name,
                                host.addr,
                                host.port,
                                host.protocol
                            );
                            if tx.send(host).is_err() {
                                break; // receiver gone — stop browsing
                            }
                        }
                    })
                    .expect("spawn mdns browse thread");
                workers.push(worker);
            }
            // Dropped here so the aggregate channel closes once the last worker's clone goes with
            // it, rather than staying open on this thread's unused copy.
            drop(tx);
            for worker in workers {
                let _ = worker.join();
            }
            tracing::debug!("mdns: receiver loop ended, shutting down");
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    Some((rx, daemon_handle))
}
