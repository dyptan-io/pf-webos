//! The Home screen: sidebar/grid navigation, host selection, the game library
//! fetch, grid scrolling, and launching a card.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
use super::*;
use crate::store::{self};
use crate::ui::{self, AddHostState, HostEntry, MenuEvent};
use std::time::Instant;

impl App {
    /// Total sidebar nav positions: host rows + "+ Add host" + "Settings".
    pub(crate) fn sidebar_len(&self) -> usize {
        self.entries.len() + 2
    }

    /// Total grid nav positions: "Desktop" + fetched games. `0` (no cards at all)
    /// only when no host is selected yet.
    pub(crate) fn grid_len(&self) -> usize {
        if self.selected_host.is_some() {
            1 + self.games.len()
        } else {
            0
        }
    }

    pub(crate) fn sidebar_index_for_selected(&self) -> usize {
        match &self.selected_host {
            Some((h, p)) => self
                .entries
                .iter()
                .position(|e| e.host() == h && e.port() == *p)
                .unwrap_or(0),
            None => 0,
        }
    }
    /// The sidebar focus for row `index`, staying on the ⋯ column when `prefer_menu`
    /// and that row actually has one (only host rows do).
    pub(crate) fn sidebar_focus_for(index: usize, host_count: usize, prefer_menu: bool) -> HomeFocus {
        if prefer_menu && index < host_count {
            HomeFocus::SidebarMenu(index)
        } else {
            HomeFocus::Sidebar(index)
        }
    }

    /// Handles one menu event on the Home screen (sidebar + grid). Returns a
    /// `ConnectTarget` when a grid card is confirmed.
    pub fn handle_home_event(&mut self, ev: MenuEvent, screen_w: u32, screen_h: u32) -> Option<ConnectTarget> {
        let sidebar_len = self.sidebar_len();
        let grid_len = self.grid_len();
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);

        match ev {
            MenuEvent::Up => match &mut self.home_focus {
                HomeFocus::Sidebar(i) => *i = if *i == 0 { sidebar_len - 1 } else { *i - 1 },
                // Walking up the ⋯ column stays on it while the row above is still a
                // host row; stepping off the top of the host list falls back to the row
                // itself, since the utility rows have no actions button.
                HomeFocus::SidebarMenu(i) => {
                    let next = if *i == 0 { sidebar_len - 1 } else { *i - 1 };
                    self.home_focus = Self::sidebar_focus_for(next, self.entries.len(), true);
                }
                HomeFocus::Grid(i) => {
                    if *i >= columns {
                        *i -= columns;
                        let i = *i;
                        self.ensure_grid_visible(i, columns, screen_w, screen_h);
                    }
                }
            },
            MenuEvent::Down => match &mut self.home_focus {
                HomeFocus::Sidebar(i) => *i = (*i + 1) % sidebar_len,
                HomeFocus::SidebarMenu(i) => {
                    let next = (*i + 1) % sidebar_len;
                    self.home_focus = Self::sidebar_focus_for(next, self.entries.len(), true);
                }
                HomeFocus::Grid(i) => {
                    let next = *i + columns;
                    if next < grid_len {
                        *i = next;
                        self.ensure_grid_visible(next, columns, screen_w, screen_h);
                    }
                }
            },
            MenuEvent::Left => {
                if let HomeFocus::SidebarMenu(i) = self.home_focus {
                    self.home_focus = HomeFocus::Sidebar(i);
                } else if let HomeFocus::Grid(i) = self.home_focus {
                    if i % columns == 0 {
                        self.home_focus = HomeFocus::Sidebar(self.sidebar_index_for_selected());
                    } else {
                        self.home_focus = HomeFocus::Grid(i - 1);
                        self.ensure_grid_visible(i - 1, columns, screen_w, screen_h);
                    }
                }
            }
            MenuEvent::Right => match self.home_focus {
                // A host row's first Right lands on its ⋯ button rather than jumping
                // straight to the grid — that button is the whole point of the
                // affordance, and it must be reachable without a pointer.
                HomeFocus::Sidebar(i) if i < self.entries.len() => {
                    self.home_focus = HomeFocus::SidebarMenu(i);
                }
                HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_) => {
                    if grid_len > 0 {
                        self.home_focus = HomeFocus::Grid(0);
                        self.ensure_grid_visible(0, columns, screen_w, screen_h);
                    }
                }
                HomeFocus::Grid(i) => {
                    if (i + 1) % columns != 0 && i + 1 < grid_len {
                        self.home_focus = HomeFocus::Grid(i + 1);
                        self.ensure_grid_visible(i + 1, columns, screen_w, screen_h);
                    }
                }
            },
            MenuEvent::Confirm => match self.home_focus {
                HomeFocus::Sidebar(i) if i < self.entries.len() => {
                    self.confirm_sidebar_host(i);
                }
                HomeFocus::Sidebar(i) if i == self.entries.len() => {
                    self.add_host = AddHostState::default();
                    self.screen = Screen::AddHost;
                }
                HomeFocus::Sidebar(_) => {
                    self.screen = Screen::Settings;
                    self.dropdown = None;
                    self.settings_focused = 0;
                    self.settings_scroll = 0;
                    self.settings_scroll_shown_at = None;
                }
                HomeFocus::SidebarMenu(i) => self.open_host_menu(i),
                HomeFocus::Grid(i) => self.confirm_grid_card(i),
            },
            // Forgets the focused host (removes its persisted entry/fingerprint —
            // it'll reappear as "not paired" if still discoverable on the LAN).
            MenuEvent::Secondary => {
                if let HomeFocus::Sidebar(i) = self.home_focus {
                    if i < self.entries.len() {
                        self.forget_host(i);
                    }
                }
            }
            MenuEvent::Back => {}
        }
        None
    }
    /// The largest useful `grid_scroll` for the current library/layout — 0 when
    /// everything already fits on screen.
    pub(crate) fn max_grid_scroll(&self, columns: usize, available_w: u32, screen_h: u32) -> i32 {
        let viewport_h = screen_h as i32 - ui::GRID_PAD - ui::GRID_TOP_Y;
        (ui::grid_layer_height(self.grid_len(), columns, available_w) as i32 - 2 * ui::GRID_LAYER_PAD - viewport_h)
            .max(0)
    }

    /// Scrolls the grid (via `grid_scroll_target` — the rendered offset eases
    /// toward it, see `tick_animations`) just far enough that focused card `idx`,
    /// including its focus-ring halo, will be fully on screen; also starts the
    /// focus pop, since this is called on exactly the moves that change grid
    /// focus. Clamped to the grid's real extent.
    pub(crate) fn ensure_grid_visible(&mut self, idx: usize, columns: usize, screen_w: u32, screen_h: u32) {
        /// Focus ring + `inflate` overhang around a focused card, plus a little
        /// breathing room.
        const FOCUS_MARGIN: i32 = 16;
        self.focus_anim = Some(Instant::now());
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let r = ui::grid_card_rect(idx, columns, ui::SIDEBAR_W as i32, available_w);
        let viewport_top = ui::GRID_TOP_Y;
        let viewport_bottom = screen_h as i32 - ui::GRID_PAD;
        let max_scroll = self.max_grid_scroll(columns, available_w, screen_h);
        let card_top = r.y() - FOCUS_MARGIN;
        let card_bottom = r.y() + r.height() as i32 + FOCUS_MARGIN;
        let mut target = self.grid_scroll_target;
        if card_top - target < viewport_top {
            target = card_top - viewport_top;
        } else if card_bottom - target > viewport_bottom {
            target = card_bottom - viewport_bottom;
        }
        self.grid_scroll_target = target.clamp(0, max_scroll);
    }

    /// Scrolls the grid by `dy_px` (positive = content moves up), clamped — the
    /// Magic Remote's scroll wheel on the Home screen. Returns whether the target
    /// actually moved (drives redraw; the eased offset follows in
    /// `tick_animations`).
    pub fn scroll_grid_by(&mut self, dy_px: i32, screen_w: u32, screen_h: u32) -> bool {
        if self.selected_host.is_none() {
            return false;
        }
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);
        let max_scroll = self.max_grid_scroll(columns, available_w, screen_h);
        let next = (self.grid_scroll_target + dy_px).clamp(0, max_scroll);
        let changed = next != self.grid_scroll_target;
        self.grid_scroll_target = next;
        changed
    }
    pub(crate) fn confirm_sidebar_host(&mut self, idx: usize) {
        let entry = self.entries[idx].clone();
        match entry {
            HostEntry::Known(h) if h.fingerprint.is_some() => {
                let (host, port, mgmt_port) = (h.host, h.port, h.mgmt_port);
                // Re-confirming the already-active host refreshes its library too — a
                // user clicking it is asking to see the current game list, e.g. after
                // installing something new on the host.
                self.select_host(host, port, mgmt_port);
            }
            _ => self.open_pairing(idx),
        }
    }

    /// Makes `(host, port)` the active sidebar selection and kicks off an async
    /// (re)fetch of its game library via `library::load_games_async` — see
    /// `drain_games` for where the result lands. Used to call `fetch_games`
    /// directly, right here, blocking: a real network round-trip (up to the
    /// 5s connect / 10s total timeout `library::agent` sets) on the same thread
    /// that pumps SDL events and renders, freezing all input — button presses,
    /// pointer motion, everything — for as long as the host took to answer or
    /// time out. `App::new` calls this synchronously-in-spirit-only at startup
    /// too (restoring the last-selected host), so that froze every launch just
    /// the same.
    pub(crate) fn select_host(&mut self, host: String, port: u16, mgmt_port: Option<u16>) {
        let _ = store::save_selected_host(&host, port);
        self.selected_host = Some((host.clone(), port));
        self.home_status = Some("Loading library…".into());
        self.games = Vec::new();
        self.art.clear();
        // Dropping the loader stops its worker (its request channel closes), so a host
        // switch abandons in-flight fetches for the previous library.
        self.art_loader = None;
        self.home_focus = HomeFocus::Grid(0);
        self.sidebar_dirty = true;
        self.grid_dirty = true;
        self.grid_scroll = 0;
        self.grid_scroll_target = 0;

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let fingerprint = self
            .known_hosts
            .iter()
            .find(|h| h.host == host && h.port == port)
            .and_then(|h| h.fingerprint);
        let mgmt_port = mgmt_port.unwrap_or(crate::library::DEFAULT_MGMT_PORT);
        tracing::debug!("library: fetching from {host}:{mgmt_port}…");
        self.games_rx = Some(crate::library::load_games_async(
            host,
            port,
            mgmt_port,
            identity,
            fingerprint,
        ));
    }

    /// Drains a finished `select_host` library fetch, if any — called alongside
    /// `drain_discovery`/`drain_art`/`tick_wake`. Returns whether anything changed.
    /// Switching hosts again before a fetch finishes discards its result safely:
    /// `select_host` already replaced `games_rx` with a fresh channel by the time
    /// this could run, so there's nothing here to receive from for the stale one.
    pub fn drain_games(&mut self) -> bool {
        let Some(rx) = &self.games_rx else { return false };
        let Ok(loaded) = rx.try_recv() else { return false };
        self.games_rx = None;
        let crate::library::GamesLoaded {
            host,
            port,
            mgmt_port,
            result,
        } = loaded;
        match result {
            Ok(mut games) => {
                // The host returns its own scan order, which is neither stable nor
                // meaningful to a reader. On a TV the grid is navigated a card at a time
                // with a d-pad, so alphabetical is the difference between "find the game"
                // and "sweep the whole library". Case-insensitive so casing doesn't
                // scatter otherwise-adjacent titles.
                games.sort_by_key(|g| g.title.to_lowercase());
                tracing::info!("library: {} games from {host}:{mgmt_port}", games.len());
                let identity = (self.identity.0.clone(), self.identity.1.clone());
                let fingerprint = self
                    .known_hosts
                    .iter()
                    .find(|h| h.host == host && h.port == port)
                    .and_then(|h| h.fingerprint);
                // Covers are requested per card as the grid window reaches them (see
                // `App::prepare_tiles`), not fetched for the whole library up front.
                self.art_loader = Some(crate::art::ArtLoader::spawn(
                    host,
                    port,
                    mgmt_port,
                    identity,
                    fingerprint,
                ));
                self.games = games;
                self.home_status = None;
            }
            Err(e) => {
                tracing::warn!("library fetch failed ({host}:{mgmt_port}): {e}");
                self.handle_library_error(host, port, e);
            }
        }
        self.grid_dirty = true;
        true
    }

    /// Shared handling for a failed library fetch/reachability check, used by both
    /// `drain_games` and `drain_launch_check`. `Unreachable` opens the Wake dialog
    /// (even with no MAC on record — `start_wake`/`render_wake` just hide the send
    /// controls then); `NotPaired`/`PinMismatch`/`Http` mean the host answered, so
    /// Wake-on-LAN wouldn't help — those stay a plain status line.
    pub(crate) fn handle_library_error(&mut self, host: String, port: u16, e: crate::library::LibraryError) {
        let reason = format!("{e} (Desktop is still available.)");
        if matches!(e, crate::library::LibraryError::Unreachable(_)) {
            let mac = self
                .known_hosts
                .iter()
                .find(|h| h.host == host && h.port == port)
                .map(|h| h.mac.clone())
                .unwrap_or_default();
            self.start_wake(host, port, mac, reason);
        } else {
            self.home_status = Some(reason);
        }
    }
    /// Confirms a grid card ("Desktop" at `idx == 0`, or a game). Kicks off a fresh
    /// reachability check first rather than handing back a `ConnectTarget` directly —
    /// the grid being populated only proves the host answered once, when its library
    /// was last fetched, and it could have gone offline since (`session::connect`'s
    /// failure currently propagates uncaught, taking the whole process down — see
    /// `main.rs`'s docs). `main.rs`'s tick loop drains the result via
    /// `drain_launch_check`/`take_ready_launch`. No-ops if a check is already in flight.
    pub(crate) fn confirm_grid_card(&mut self, idx: usize) {
        if self.pending_launch.is_some() {
            return;
        }
        let Some((host, port)) = self.selected_host.clone() else {
            return;
        };
        let Some(known) = self.known_hosts.iter().find(|h| h.host == host && h.port == port) else {
            return;
        };
        let Some(fingerprint) = known.fingerprint else { return };
        let launch = if idx == 0 {
            None
        } else {
            let Some(game) = self.games.get(idx - 1) else { return };
            Some(game.id.clone())
        };
        let mgmt_port = known.mgmt_port.unwrap_or(crate::library::DEFAULT_MGMT_PORT);
        let identity = (self.identity.0.clone(), self.identity.1.clone());
        tracing::debug!("launch: checking {host}:{port} is still reachable before connecting…");
        self.home_status = Some("Checking connection…".into());
        let rx = crate::library::load_games_async(host.clone(), port, mgmt_port, identity, Some(fingerprint));
        self.pending_launch = Some(PendingLaunch {
            host,
            port,
            fingerprint,
            launch,
            rx,
        });
    }

    /// Drains `confirm_grid_card`'s pre-flight reachability check, if it's finished. On
    /// success, stashes the result in `launch_ready` for `main.rs` to pick up via
    /// `take_ready_launch` (dropped instead if the selection has since moved to a
    /// different host). On failure, defers to `handle_library_error`.
    pub fn drain_launch_check(&mut self) -> bool {
        let Some(pending) = &self.pending_launch else {
            return false;
        };
        let Ok(loaded) = pending.rx.try_recv() else {
            return false;
        };
        let PendingLaunch {
            host,
            port,
            fingerprint,
            launch,
            ..
        } = self.pending_launch.take().expect("just matched Some above");
        match loaded.result {
            Ok(_) => {
                if self
                    .selected_host
                    .as_ref()
                    .is_some_and(|(h, p)| *h == host && *p == port)
                {
                    self.home_status = None;
                    self.launch_ready = Some(ConnectTarget {
                        host,
                        port,
                        fingerprint,
                        launch,
                    });
                }
            }
            Err(e) => {
                tracing::warn!("launch check failed ({host}:{port}): {e}");
                self.handle_library_error(host, port, e);
            }
        }
        self.sidebar_dirty = true;
        self.grid_dirty = true;
        true
    }

    /// Takes the `ConnectTarget` a finished `drain_launch_check` produced, if any —
    /// `main.rs`'s tick loop calls this right after `drain_launch_check` and breaks its
    /// event loop with it to actually start the stream.
    pub fn take_ready_launch(&mut self) -> Option<ConnectTarget> {
        self.launch_ready.take()
    }
    pub(crate) fn forget_host(&mut self, idx: usize) {
        let HostEntry::Known(h) = &self.entries[idx] else {
            return;
        };
        let (host, port) = (h.host.clone(), h.port);
        crate::art::clear_host_cache(&host, port);
        self.known_hosts.retain(|k| !(k.host == host && k.port == port));
        let _ = store::save_known_hosts(&self.known_hosts);
        self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
        if self.selected_host.as_ref() == Some(&(host, port)) {
            self.selected_host = None;
            self.games = Vec::new();
            self.home_status = None;
            self.home_focus = HomeFocus::Sidebar(0);
        }
        let sidebar_len = self.sidebar_len();
        if let HomeFocus::Sidebar(i) = &mut self.home_focus {
            if *i >= sidebar_len {
                *i = sidebar_len - 1;
            }
        }
        self.sidebar_dirty = true;
        self.grid_dirty = true;
    }
}
