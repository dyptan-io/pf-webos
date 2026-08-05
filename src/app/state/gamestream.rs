//! The two `GameStream`-only host asides that need a worker thread and a status line: ending the
//! app a host is still running, and finding out whether a hand-typed address is a `GameStream`
//! host rather than a punktfunk one. Both share one shape — a blocking host call on a thread, a
//! single message back, drained into `home_status` on the next tick — gated on
//! `Settings::gamestream_enabled`.
use crate::app::App;
use crate::core::protocol::Protocol;
use crate::services::store;
use crate::ui::HostEntry;

/// The `GameStream` HTTP port a manually added host is tried on. Not a setting: `47989` is the
/// port Sunshine, Apollo and Wolf all serve plain HTTP on, and a host that moved it is not
/// discoverable by any Moonlight client either.
const GS_PROBE_PORT: u16 = crate::backend::gamestream::DEFAULT_HTTP_PORT;

/// One finished manual-address probe. Only ever sent when the address answered as a `GameStream`
/// host — a probe that finds nothing leaves the punktfunk record exactly as typed.
pub(crate) struct GsProbed {
    pub(crate) host: String,
    /// The port the record was saved with, so the rewrite finds the right one.
    pub(crate) port: u16,
    /// What the host calls itself, for the sidebar row.
    pub(crate) name: String,
}

impl App {
    /// Whether a discovered `GameStream` row for `addr` should stay hidden, because the same
    /// machine also speaks punktfunk *and* that side is usable right now.
    ///
    /// punktfunk is the better half of a host that offers both (full feature set), so pairing
    /// with its `GameStream` side is only worth offering when the punktfunk one can't be
    /// reached. "Can't be reached" means a probe that actually came back negative — an
    /// unprobed host is assumed up, so a freshly resolved advert doesn't flash a second row
    /// before the first sweep lands. A *saved* `GameStream` host is never shadowed: the user
    /// paired it deliberately, and its row carries its own online dot.
    pub(crate) fn gamestream_shadowed(&self, addr: &str) -> bool {
        self.ports_at(addr, Protocol::Punktfunk)
            .into_iter()
            .any(|port| self.reachable.get(&(addr.to_string(), port)) != Some(&false))
    }

    /// Parks a discovered `GameStream` host instead of giving it a row, keeping the list free of
    /// duplicates. See [`Self::refresh_gamestream_shadowing`] for when it comes back out.
    pub(crate) fn park_gamestream(&mut self, host: crate::services::discovery::DiscoveredHost) {
        if !self
            .shadowed_gamestream
            .iter()
            .any(|d| d.addr == host.addr && d.port == host.port)
        {
            self.shadowed_gamestream.push(host);
        }
    }

    /// Moves discovered `GameStream` rows between the sidebar and `shadowed_gamestream` so the
    /// list matches [`Self::gamestream_shadowed`] — called after discovery and after every
    /// reachability sweep, since either can change the answer. Returns whether the sidebar moved.
    ///
    /// The caller owns the focus re-anchoring, so a drain that also appended rows re-anchors once
    /// rather than twice off two different starting lengths.
    pub(crate) fn refresh_gamestream_shadowing(&mut self) -> bool {
        if !self.settings.gamestream_enabled {
            // Nothing to promote into a sidebar that isn't showing `GameStream` at all, and the
            // parked rows are stale the moment the toggle goes off.
            self.shadowed_gamestream.clear();
            // The rows themselves are `refilter_entries`' business, so nothing here changed.
            return false;
        }
        // Both passes decide before mutating: `gamestream_shadowed` reads `entries` itself.
        let park: Vec<(String, u16)> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                HostEntry::Discovered(d) if d.protocol == Protocol::GameStream => Some((d.addr.clone(), d.port)),
                _ => None,
            })
            .filter(|(addr, _)| self.gamestream_shadowed(addr))
            .collect();
        let mut parked = Vec::new();
        self.entries.retain(|e| match e {
            HostEntry::Discovered(d)
                if d.protocol == Protocol::GameStream && park.iter().any(|(a, p)| a == &d.addr && *p == d.port) =>
            {
                parked.push(d.clone());
                false
            }
            _ => true,
        });
        for d in parked {
            self.park_gamestream(d);
        }
        // The other direction: the punktfunk side went away, so these are worth offering again.
        let unpark: Vec<crate::services::discovery::DiscoveredHost> = self
            .shadowed_gamestream
            .iter()
            .filter(|d| !self.gamestream_shadowed(&d.addr))
            .cloned()
            .collect();
        self.shadowed_gamestream
            .retain(|d| !unpark.iter().any(|u| u.addr == d.addr && u.port == d.port));
        let mut unparked = 0;
        for d in unpark {
            // Anything paired or added by hand meanwhile already has a row of its own.
            if !self.host_listed(&d.addr, d.port) {
                self.entries.push(HostEntry::Discovered(d));
                unparked += 1;
            }
        }
        let changed = !park.is_empty() || unparked > 0;
        if changed {
            self.sidebar_dirty = true;
        }
        changed
    }

    /// Re-applies the `Settings::gamestream_enabled` gate to everything already on screen, after
    /// the toggle moved.
    ///
    /// Rebuilding the sidebar is not enough on its own: with the toggle off a `GameStream` host
    /// loses its row, but a library already fetched from it would keep filling the grid — a game
    /// list belonging to a host the user can no longer see or go back to. The browse itself has to
    /// move too, or the toggle would only ever hide rows the client is still spending a thread and
    /// mDNS traffic to find.
    pub(crate) fn apply_gamestream_visibility(&mut self) {
        self.restart_discovery();
        self.refilter_entries();
        if self.settings.gamestream_enabled {
            // Nothing is parked yet (the fresh browse hasn't resolved anything), but a host that
            // resolves next is only shadowed once its punktfunk side has been probed.
            self.refresh_gamestream_shadowing();
            return;
        }
        // Parked rows belong to a protocol that is no longer browsed for, so they can never be
        // un-parked by a later sweep — dropping them is what stops the toggle leaking them back.
        self.shadowed_gamestream.clear();
        let selected_gs = self
            .selected_known_host()
            .is_some_and(|h| h.protocol == Protocol::GameStream);
        if selected_gs {
            self.clear_selected_host();
        }
    }

    /// Ends whatever the host is still running. Fire-and-forget on a worker, with the outcome
    /// reported on the Home status line: the modal closes now, so there is nowhere else to put it.
    pub(crate) fn start_quit_app(&mut self, idx: usize) {
        let Some(entry) = self.entries.get(idx) else { return };
        let backend = crate::backend::backend_for(entry.protocol());
        if !backend.caps().quit_app {
            return;
        }
        let name = entry.name().to_string();
        let addr = entry.host().to_string();
        // Same defaulting as `App::forget_host`: the record's management port, or the backend's.
        let query_port = entry.mgmt_port().unwrap_or_else(|| backend.default_query_port());
        self.host_menu_index = None;
        self.screen = crate::core::screen::Screen::Home;
        self.home_status = Some(format!("Asking {name} to close what it's running…"));
        let (tx, rx) = std::sync::mpsc::channel();
        self.quit_app_rx = Some(rx);
        std::thread::spawn(move || {
            let msg = match backend.quit_app(&addr, query_port) {
                Ok(true) => format!("{name} closed its running app."),
                // `/cancel` says nothing about *why* it refused, and an idle host and someone
                // else's session are the two possibilities — see `query::quit_running_app`.
                Ok(false) => format!("{name} had nothing running, or the session is another device's."),
                Err(e) => {
                    tracing::warn!("quit app on {addr}:{query_port} failed: {e:#}");
                    format!("Couldn't reach {name}.")
                }
            };
            let _ = tx.send(msg);
        });
    }

    /// Moves a finished quit onto the status line.
    pub(crate) fn drain_quit_app(&mut self) -> bool {
        let Some(rx) = &self.quit_app_rx else { return false };
        match rx.try_recv() {
            Ok(msg) => {
                self.home_status = Some(msg);
                self.quit_app_rx = None;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            // The worker died without sending; the "asking…" line would otherwise sit there.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.quit_app_rx = None;
                self.home_status = None;
                true
            }
        }
    }

    /// Probes a manually added address for a `GameStream` host, off-thread.
    ///
    /// `confirm_add_host` only *saves* an address — nothing pairs, so there's no failure to fall
    /// back from. Instead this probes the one port `GameStream` hosts serve; if it answers, the
    /// record just saved is rewritten to that protocol and the pairing modal picks the
    /// display-PIN layout by itself. A host that speaks neither protocol keeps the record as
    /// typed.
    pub(crate) fn probe_gamestream_fallback(&mut self, host: String, port: u16) {
        if !self.settings.gamestream_enabled {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.gs_probe_rx = Some(rx);
        std::thread::spawn(move || {
            // `open` fetches `/serverinfo`, so answering at all is the whole test — an unpaired
            // host is a perfectly normal answer here and the point of the probe.
            match crate::backend::gamestream::query::open(&host, Some(GS_PROBE_PORT)) {
                Ok(gs) => {
                    let name = gs.host_name().unwrap_or_else(|_| host.clone());
                    let _ = tx.send(GsProbed { host, port, name });
                }
                Err(e) => tracing::debug!("{host}:{GS_PROBE_PORT} is not a GameStream host: {e:#}"),
            }
        });
    }

    /// Rewrites the saved record when the probe found a `GameStream` host at the typed address.
    ///
    /// The punktfunk record is *replaced*, not joined: the user typed one address and means one
    /// host, and two rows for it would both be offered a Connect, one of which cannot work.
    pub(crate) fn drain_gamestream_probe(&mut self) -> bool {
        let Some(rx) = &self.gs_probe_rx else { return false };
        let probed = match rx.try_recv() {
            Ok(p) => p,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.gs_probe_rx = None;
                return false;
            }
        };
        self.gs_probe_rx = None;
        let Some(known) = self
            .known_hosts
            .iter_mut()
            .find(|k| k.host == probed.host && k.port == probed.port)
        else {
            // Forgotten or edited while the probe ran — the user's later action wins.
            return false;
        };
        known.protocol = Protocol::GameStream;
        known.port = GS_PROBE_PORT;
        // One port in both fields, as `parse_discovery` does for a discovered host: queries and
        // launches share it.
        known.mgmt_port = Some(GS_PROBE_PORT);
        // Only replace the name where the user has none of their own — the address is what
        // `confirm_add_host` names a manual record.
        if known.name == probed.host {
            known.name = probed.name.clone();
        }
        let _ = store::save_known_hosts(&self.known_hosts);
        self.rebuild_entries();
        self.home_status = Some(format!(
            "{} is a GameStream host — show its pairing PIN to pair.",
            probed.name
        ));
        // Keep the cursor on the row that was just rewritten; its index can have moved.
        if let Some(i) = self
            .entries
            .iter()
            .position(|e| e.host() == probed.host && matches!(e, HostEntry::Known(_)))
        {
            self.home_focus = crate::app::HomeFocus::Sidebar(i);
        }
        self.sidebar_dirty = true;
        true
    }
}
