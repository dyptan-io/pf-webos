//! Host reachability: the presence dot on each sidebar row.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
//!
//! Before this, a host that had been powered off for a week looked exactly like one that
//! was up — mDNS only ever *adds* (`discovery::browse` handles `ServiceResolved` and logs
//! every other event, `ServiceRemoved` included), and a saved host isn't discovered at all.
//! The only way to find out was to press OK and wait for a timeout, which then dropped you
//! into the Wake prompt.
//!
//! `NativeClient::probe` is a bounded, trust-agnostic, mDNS-independent QUIC handshake —
//! no identity, no pairing, no session. One background thread walks every entry on a slow
//! cadence and reports each result over a channel, in the same drain-per-tick shape as
//! discovery and cover art.
use super::*;
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
    /// Kicks off a reachability sweep if one is due and none is in flight. Returns whether
    /// anything changed (it never does here — results arrive via `drain_reachability`).
    pub(crate) fn tick_reachability(&mut self) {
        if self.reach_rx.is_some() {
            return; // a sweep is still running
        }
        if self.reach_last.is_some_and(|t| t.elapsed() < REACH_INTERVAL) {
            return;
        }
        self.reach_last = Some(Instant::now());
        let targets: Vec<(String, u16)> = self
            .entries
            .iter()
            .map(|e| (e.host().to_string(), e.port()))
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
            for (host, port) in targets {
                let online = punktfunk_core::client::NativeClient::probe(&host, port, REACH_TIMEOUT);
                if tx.send(Reachability { host, port, online }).is_err() {
                    return;
                }
            }
        });
    }

    /// Drains finished probes. Returns whether the sidebar actually changed — a host that
    /// was already known-online reporting online again must not force a repaint.
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

    /// This entry's last known reachability — `None` until it has been probed once, which
    /// the sidebar draws as "no dot" rather than guessing.
    pub(crate) fn entry_online(&self, entry: &crate::ui::HostEntry) -> Option<bool> {
        self.reachable
            .get(&(entry.host().to_string(), entry.port()))
            .copied()
    }

    /// Every entry's state, index-aligned with `entries` — what the sidebar renderer takes.
    pub(crate) fn reachability_list(&self) -> Vec<Option<bool>> {
        self.entries.iter().map(|e| self.entry_online(e)).collect()
    }

    /// Fresh state for a new `App`.
    pub(crate) fn new_reachability() -> HashMap<(String, u16), bool> {
        HashMap::new()
    }
}
