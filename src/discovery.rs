//! LAN discovery via mDNS (_punktfunk._udp). Direct mdns-sd dep to avoid pf-client-core's FFmpeg/PipeWire.
use mdns_sd::{ServiceDaemon, ServiceEvent};

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Management API port from mDNS (None → `library::DEFAULT_MGMT_PORT`).
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MACs from mDNS (learned while awake, persisted to `KnownHost`).
    pub mac: Vec<String>,
}

/// Browse continuously; returns (Receiver, `ServiceDaemon`). Thread won't exit until `ServiceDaemon::shutdown()`
/// called explicitly — mdns-sd's `SearchStarted` events loop forever. Failure points logged via tracing.
pub fn browse() -> Option<(std::sync::mpsc::Receiver<DiscoveredHost>, ServiceDaemon)> {
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
            let receiver = match daemon.browse("_punktfunk._udp.local.") {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("mdns: browse(_punktfunk._udp.local.) failed: {e}");
                    return;
                }
            };
            tracing::debug!("mdns: browsing _punktfunk._udp.local.");
            while let Ok(event) = receiver.recv() {
                let info = match event {
                    ServiceEvent::ServiceResolved(info) => info,
                    other => {
                        tracing::debug!("mdns: {other:?}");
                        continue;
                    }
                };
                // IPv4 only (same as other clients)
                let Some(addr) = info
                    .get_addresses_v4()
                    .iter()
                    .next()
                    .map(std::string::ToString::to_string)
                else {
                    tracing::warn!("mdns: resolved {} with no IPv4 address, skipping", info.get_fullname());
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
                tracing::info!("mdns: resolved {} at {}:{}", host.name, host.addr, host.port);
                if tx.send(host).is_err() {
                    break; // receiver gone — stop browsing
                }
            }
            tracing::debug!("mdns: receiver loop ended, shutting down");
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    Some((rx, daemon_handle))
}
