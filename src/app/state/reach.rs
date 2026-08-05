//! Ambient reachability polling for sidebar host rows. Pure logic — no view counterpart.
use crate::app::App;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How often the whole host list is re-probed. Deliberately slow: this is ambient status, nobody
/// waits on it, and every round is a connection the host logs — at 30 s a `GameStream` host's log
/// was mostly us. The cost of a longer interval is only that the dot (and the `GameStream`
/// shadowing rule that reads it) lags a host coming up or going down by up to this long.
const REACH_INTERVAL: Duration = Duration::from_secs(180);

/// One host's probe result.
pub(crate) struct Reachability {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) online: bool,
    /// MAC(s) learned from the host itself during this probe — only ever non-empty for a
    /// protocol whose MAC doesn't arrive with discovery, and only until one is stored.
    pub(crate) mac: Vec<String>,
}

impl App {
    /// Kick off reachability sweep if one is due and none is in flight.
    pub(crate) fn tick_reachability(&mut self) {
        if self.reach_rx.is_some() {
            return; // a sweep is still running
        }
        if self.reach_last.is_some_and(|t| t.elapsed() < REACH_INTERVAL) {
            return;
        }
        self.reach_last = Some(Instant::now());
        // Probed through the entry's own backend: a `GameStream` host would fail a punktfunk
        // handshake and read as offline forever.
        // The `bool` is "also ask this host for its MAC": a `GameStream` host advertises none
        // over mDNS, so a record paired before `wake_mac` existed (or one whose pairing pass
        // couldn't reach `/serverinfo`) would never have a MAC to wake. Asked only while the
        // record still lacks one, so this settles to a plain probe after the first success.
        let targets: Vec<(crate::core::protocol::Protocol, String, u16, bool)> = self
            .entries
            .iter()
            .map(|e| (e.protocol(), e.host().to_string(), e.port(), e.mac().is_empty()))
            .collect();
        if targets.is_empty() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.reach_rx = Some(rx);
        // One thread for the whole sweep, probing sequentially: the host count here is a
        // handful, and a thread per host would spike this SoC's 3 cores for a cosmetic
        // indicator. Each send failing (the receiver replaced by a newer sweep, or the app
        // gone) just ends the sweep early.
        std::thread::spawn(move || {
            for (protocol, host, port, want_mac) in targets {
                let backend = crate::backend::backend_for(protocol);
                let online = backend.probe(&host, port, crate::services::budget::PROBE);
                // Only when it answered: `wake_mac` is a real request on `budget::REQUEST`, far
                // longer than the probe above, and an offline host is exactly the one that can't
                // tell us how to wake it.
                let mac = if online && want_mac {
                    backend.wake_mac(&host, port)
                } else {
                    Vec::new()
                };
                if tx
                    .send(Reachability {
                        host,
                        port,
                        online,
                        mac,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    /// Drain finished probes. Returns true if sidebar changed.
    pub(crate) fn drain_reachability(&mut self) -> bool {
        let Some(rx) = &self.reach_rx else { return false };
        let mut changed = false;
        let mut finished = false;
        // Held until the loop ends: the receiver borrows `self`, so nothing that needs `&mut self`
        // can run inside it.
        let mut learned = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(r) => {
                    if !r.mac.is_empty() {
                        learned.push((r.host.clone(), r.port, r.mac));
                    }
                    let key = (r.host, r.port);
                    if self.reachable.get(&key) != Some(&r.online) {
                        self.reachable.insert(key, r.online);
                        changed = true;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.reach_rx = None;
        }
        for (host, port, mac) in learned {
            changed |= self.learn_wake_mac(&host, port, mac);
        }
        if changed {
            self.sidebar_dirty = true;
        }
        // A punktfunk host that just went offline un-shadows its machine's `GameStream` side,
        // and one that came back re-shadows it — which moves the sidebar's utility rows.
        let before = self.entries.len();
        if self.refresh_gamestream_shadowing() {
            self.reanchor_sidebar_focus(before);
            changed = true;
        }
        changed
    }

    /// Records a MAC a host reported about itself, persisting it. Both the stored record and the
    /// live sidebar entry are updated: the entry's copy is what stops the next sweep asking again.
    /// Returns true if anything took it — a host with no record of its own (discovered, never
    /// paired) has nowhere to keep it.
    fn learn_wake_mac(&mut self, host: &str, port: u16, mac: Vec<String>) -> bool {
        if let Some(crate::ui::HostEntry::Known(k)) =
            self.entries.iter_mut().find(|e| e.host() == host && e.port() == port)
        {
            k.mac.clone_from(&mac);
        }
        let Some(known) = self.known_hosts.iter_mut().find(|h| h.host == host && h.port == port) else {
            return false;
        };
        tracing::info!("host {host}:{port}: learned wake MAC {mac:?}");
        known.mac = mac;
        let _ = crate::services::store::save_known_hosts(&self.known_hosts);
        true
    }

    /// Last known reachability (None until first probe).
    pub(crate) fn entry_online(&self, entry: &crate::ui::HostEntry) -> Option<bool> {
        self.reachable.get(&(entry.host().to_string(), entry.port())).copied()
    }

    /// All reachability states, index-aligned with entries.
    pub(crate) fn reachability_list(&self) -> Vec<Option<bool>> {
        self.entries.iter().map(|e| self.entry_online(e)).collect()
    }

    /// Initialize empty reachability map.
    pub(crate) fn new_reachability() -> HashMap<(String, u16), bool> {
        HashMap::new()
    }
}
