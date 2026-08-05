//! LAN discovery via mDNS. Direct mdns-sd dep to avoid pf-client-core's FFmpeg/PipeWire.
//!
//! One daemon browses one service type per enabled backend, and the type a record arrived on is
//! what sets its protocol — hosts are never probed to find out what they speak.
use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::core::protocol::Protocol;

/// How long the round-robin below sleeps after a pass that found nothing on any browse. Only
/// paces the idle loop: a pass that found anything goes straight round again.
const BROWSE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

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
            // One receiver per service type, all fed by the one daemon, each remembering which
            // backend owns it so a resolved record is parsed by the protocol that advertised it.
            let mut browses = Vec::new();
            for backend in backends {
                let service = backend.discovery_service();
                match daemon.browse(service) {
                    Ok(r) => {
                        tracing::debug!("mdns: browsing {service}");
                        browses.push((backend, r));
                    }
                    Err(e) => tracing::error!("mdns: browse({service}) failed: {e}"),
                }
            }
            if browses.is_empty() {
                return;
            }
            // Round-robin rather than a blocking `recv`: with more than one browse, blocking on
            // the first would starve the others.
            'outer: loop {
                let mut idle = true;
                for (backend, receiver) in &browses {
                    // `try_recv` + an explicit idle sleep rather than `recv_timeout`: naming
                    // `RecvTimeoutError`'s variants would make `flume` a direct dependency, and
                    // `mdns-sd` re-exports `Receiver` but not its error types. Disconnection is
                    // asked of the receiver instead, which is on the re-exported type.
                    let Ok(event) = receiver.try_recv() else {
                        if receiver.is_disconnected() {
                            break 'outer; // daemon gone; nothing will ever arrive again
                        }
                        continue; // empty: try the next browse
                    };
                    idle = false;
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
                        break 'outer; // receiver gone — stop browsing
                    }
                }
                if idle {
                    std::thread::sleep(BROWSE_POLL);
                }
            }
            tracing::debug!("mdns: receiver loop ended, shutting down");
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    Some((rx, daemon_handle))
}
