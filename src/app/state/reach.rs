//! Ambient reachability polling for sidebar host rows. Pure logic — no view counterpart.
use crate::app::App;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How often the whole host list is re-probed. Deliberately slow: this is ambient status,
/// not something anyone waits on, and each round costs one handshake attempt per host.
const REACH_INTERVAL: Duration = Duration::from_secs(30);
/// Per-host handshake budget. Short — an unreachable host on a LAN fails fast (no route /
/// refused), and a host slow enough to miss this is not meaningfully "available" anyway.
const REACH_TIMEOUT: Duration = Duration::from_secs(2);

/// One host's probe result.
pub(crate) struct Reachability {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) online: bool,
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
        let targets: Vec<(String, u16)> = self.entries.iter().map(|e| (e.host().to_string(), e.port())).collect();
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
            for (host, port) in targets {
                let online = punktfunk_core::client::NativeClient::probe(&host, port, REACH_TIMEOUT);
                if tx.send(Reachability { host, port, online }).is_err() {
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
        loop {
            match rx.try_recv() {
                Ok(r) => {
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
        if changed {
            self.sidebar_dirty = true;
        }
        changed
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
