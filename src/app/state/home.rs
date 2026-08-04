//! Home screen logic: sidebar/grid navigation, host selection, game library fetch,
//! launching. Grid pixel geometry (rect helpers) lives in `app::view::home`.
use crate::app::App;
use crate::app::{ConnectTarget, GridCard, GridLayout};
use crate::core::screen::{HomeFocus, Screen};
use crate::services::store::{self};
use crate::ui::{self, AddHostState, HostEntry, MenuEvent};
use std::time::Instant;

impl App {
    /// Total sidebar nav positions: host rows + "+ Add host" + "Settings".
    pub(crate) fn sidebar_len(&self) -> usize {
        self.entries.len() + 2
    }

    /// Grid shape at `columns` columns; scans for pinned pins, so build once and reuse.
    pub(crate) fn grid_layout(&self, columns: usize) -> GridLayout {
        let desktop_pinned = self.games_loaded
            && self
                .selected_known_host()
                .is_some_and(|h| h.is_pinned(store::DESKTOP_PIN_ID));
        let front_count = self.pinned_count + usize::from(desktop_pinned);
        let pinned_rows = if front_count == 0 {
            0
        } else {
            front_count.div_ceil(columns.max(1))
        };
        GridLayout {
            pinned_count: self.pinned_count,
            desktop_pinned,
            desktop_in_rest: self.games_loaded && !desktop_pinned,
            front_count,
            pinned_rows,
            unpinned_start: pinned_rows * columns.max(1),
        }
    }

    /// Total grid nav positions — `0` (no cards at all) only when no host is
    /// selected yet, or one's selected but hasn't answered a library fetch yet.
    pub(crate) fn grid_len(&self, columns: usize) -> usize {
        if self.selected_host.is_none() {
            return 0;
        }
        self.grid_layout(columns).len(self.games.len())
    }

    pub(crate) fn pinned_rows(&self, columns: usize) -> usize {
        self.grid_layout(columns).pinned_rows
    }

    /// The card at grid index `idx`, or `None` for the padding after a partial
    /// pinned row, or out of range.
    pub(crate) fn grid_card_at(&self, idx: usize, columns: usize) -> Option<GridCard<'_>> {
        self.grid_layout(columns).card_at(&self.games, idx)
    }

    /// The pin id for whatever's at grid index `idx` — see `GridLayout::pin_id_at`.
    pub(crate) fn pin_id_at_grid_idx(&self, idx: usize, columns: usize) -> Option<&str> {
        self.grid_layout(columns).pin_id_at(&self.games, idx)
    }

    /// Inverse of `pin_id_at_grid_idx`: grid index for a pin ID, keeping focus after reorder.
    pub(crate) fn grid_idx_for_pin_id(&self, id: &str, columns: usize) -> Option<usize> {
        self.grid_layout(columns).idx_for_pin_id(&self.games, id)
    }

    /// Whether grid index `idx` is an actual card rather than empty padding
    /// after a partial pinned row.
    pub(crate) fn is_grid_card(&self, idx: usize, columns: usize) -> bool {
        self.grid_card_at(idx, columns).is_some()
    }

    pub(crate) fn sidebar_index_for_selected(&self) -> usize {
        self.sidebar_index_of_selected_host().unwrap_or(0)
    }

    /// Like `sidebar_index_for_selected`, but `None` both when nothing is selected
    /// and when the selected host has since dropped out of `entries` — a caller
    /// highlighting the active row must not fall back to row 0 in that case.
    pub(crate) fn sidebar_index_of_selected_host(&self) -> Option<usize> {
        let (h, p) = self.selected_host.as_ref()?;
        self.entries.iter().position(|e| e.host() == h && e.port() == *p)
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
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);
        let grid_len = self.grid_len(columns);

        match ev {
            MenuEvent::Up => match self.home_focus {
                HomeFocus::Sidebar(i) => {
                    self.home_focus = HomeFocus::Sidebar(if i == 0 { sidebar_len - 1 } else { i - 1 });
                }
                // Walking up the ⋯ column stays on it while the row above is still a
                // host row; stepping off the top of the host list falls back to the row
                // itself, since the utility rows have no actions button.
                HomeFocus::SidebarMenu(i) => {
                    let next = if i == 0 { sidebar_len - 1 } else { i - 1 };
                    self.home_focus = Self::sidebar_focus_for(next, self.entries.len(), true);
                }
                HomeFocus::Grid(i) => {
                    // The cell directly above can be empty padding after a partial
                    // pinned row (see `is_grid_card`) — nothing to land on there.
                    if i >= columns && self.is_grid_card(i - columns, columns) {
                        let next = i - columns;
                        self.home_focus = HomeFocus::Grid(next);
                        self.ensure_grid_visible(next, columns, screen_w, screen_h);
                    }
                }
            },
            MenuEvent::Down => match self.home_focus {
                HomeFocus::Sidebar(i) => self.home_focus = HomeFocus::Sidebar((i + 1) % sidebar_len),
                HomeFocus::SidebarMenu(i) => {
                    let next = (i + 1) % sidebar_len;
                    self.home_focus = Self::sidebar_focus_for(next, self.entries.len(), true);
                }
                HomeFocus::Grid(i) => {
                    let next = i + columns;
                    if next < grid_len && self.is_grid_card(next, columns) {
                        self.home_focus = HomeFocus::Grid(next);
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
                    // The next cell can be empty padding after a partial pinned row
                    // (see `is_grid_card`) — nothing to land on there.
                    if (i + 1) % columns != 0 && i + 1 < grid_len && self.is_grid_card(i + 1, columns) {
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
                    self.scroll = ui::ScrollWindow::new();
                    self.content_window = ui::ContentWindow::new();
                }
                HomeFocus::SidebarMenu(i) => self.open_host_menu(i),
                HomeFocus::Grid(i) => self.confirm_grid_card(i, columns),
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
            // Back on Home is owned by `App::back` (grid/⋯ → sidebar), reached via
            // `dispatch_menu_event` before this handler — never routed here.
            MenuEvent::Back => {}
        }
        None
    }

    /// Pin ID of focused grid card, or `None` for sidebar/padding.
    pub(crate) fn focused_pin_id(&self, columns: usize) -> Option<&str> {
        match self.home_focus {
            HomeFocus::Grid(idx) => self.pin_id_at_grid_idx(idx, columns),
            HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_) => None,
        }
    }

    /// Toggles focused card pin state and reorders the grid; opens pin-limit
    /// alert on overflow.
    pub(crate) fn toggle_focused_pin(&mut self, screen_w: u32, screen_h: u32) {
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);
        let HomeFocus::Grid(old_idx) = self.home_focus else {
            return;
        };
        let Some(id) = self.pin_id_at_grid_idx(old_idx, columns).map(str::to_string) else {
            return;
        };
        let Some(known) = self.selected_known_host() else {
            return;
        };
        if !known.can_toggle_pin(&id) {
            // At MAX_PINNED_GAMES already — explain instead of a silent no-op.
            self.open_pin_limit();
            return;
        }
        let was_pinned = known.is_pinned(&id);

        let Some(known) = self.selected_known_host_mut() else {
            return;
        };
        known.toggle_pin(&id);
        let _ = store::save_known_hosts(&self.known_hosts);

        self.reorder_games_by_pin();
        if let Some(new_idx) = self.grid_idx_for_pin_id(&id, columns) {
            self.home_focus = HomeFocus::Grid(new_idx);
            self.ensure_grid_visible(new_idx, columns, screen_w, screen_h);
        }
        self.replay_reorder_pop(&id, was_pinned, columns);
    }

    /// Reorder's appear animation — the same "every card pops in together" look
    /// as a fresh library reveal (see `grid_reveal_ready`), scoped to what
    /// actually needs it: the newly pinned card alone (top row — an already-
    /// pinned card that just changed order doesn't replay), plus every card in
    /// the unpinned "rest" section, which reshuffles regardless of direction.
    /// Card tiles themselves need no rebuilding either way — they're keyed by
    /// pin id (see `card_tiles`), which reordering never changes.
    fn replay_reorder_pop(&mut self, id: &str, was_pinned: bool, columns: usize) {
        let now = Instant::now();
        let layout = self.grid_layout(columns);
        let rest_ids: Vec<String> = (layout.unpinned_start..layout.len(self.games.len()))
            .filter_map(|idx| layout.pin_id_at(&self.games, idx).map(str::to_string))
            .collect();
        // Re-arm the pop clock unconditionally (not gated on a built tile like the old
        // per-`CardTile` clock): a not-yet-built card has no visible pop to replay, and
        // its clock is overwritten with a fresh one when `prepare_grid` builds it.
        if !was_pinned {
            self.card_pop.insert(id.to_string(), now);
        }
        for pin_id in rest_ids {
            self.card_pop.insert(pin_id, now);
        }
    }

    /// Re-sorts games: pinned first (in pin order), rest untouched. Also prunes
    /// `known_hosts`' persisted pins for games the host no longer lists — otherwise
    /// a removed game keeps counting toward `MAX_PINNED_GAMES` forever.
    pub(crate) fn reorder_games_by_pin(&mut self) {
        let Some(known_idx) = self
            .selected_host
            .as_ref()
            .and_then(|(h, p)| self.known_hosts.iter().position(|k| k.host == *h && k.port == *p))
        else {
            self.pinned_count = 0;
            return;
        };
        let pinned_ids = self.known_hosts[known_idx].pinned.clone();
        let mut pinned = Vec::new();
        let mut still_pinned = Vec::new();
        for id in &pinned_ids {
            // Desktop isn't in `self.games`, so it's never "missing".
            if id == store::DESKTOP_PIN_ID {
                still_pinned.push(id.clone());
            } else if let Some(pos) = self.games.iter().position(|g| &g.id == id) {
                pinned.push(self.games.remove(pos));
                still_pinned.push(id.clone());
            }
        }
        self.pinned_count = pinned.len();
        pinned.append(&mut self.games);
        self.games = pinned;

        if still_pinned.len() != pinned_ids.len() {
            self.known_hosts[known_idx].pinned = still_pinned;
            let _ = store::save_known_hosts(&self.known_hosts);
        }
    }

    /// Eased 0..=1 progress of pin id `id`'s zoom-in (see `card_pop`)
    /// — 1.0, full size, for anything not animating.
    pub(crate) fn card_pop_frac(&self, id: &str) -> f32 {
        ui::anim_frac(self.card_pop.get(id).copied(), crate::app::CARD_POP)
    }

    /// Whether the pinned front block is followed by anything — false when
    /// nothing's pinned, and when *everything* is, which would otherwise leave
    /// the divider and its gap hanging under the last row.
    pub(crate) fn has_pinned_divider(&self, columns: usize) -> bool {
        let layout = self.grid_layout(columns);
        layout.pinned_rows > 0 && layout.len(self.games.len()) > layout.unpinned_start
    }

    /// The largest useful `grid_scroll` for the current library/layout — 0 when
    /// everything already fits on screen.
    pub(crate) fn max_grid_scroll(&self, columns: usize, available_w: u32, screen_h: u32) -> i32 {
        let viewport_h = screen_h as i32 - ui::GRID_PAD - ui::GRID_TOP_Y;
        let extra = if self.has_pinned_divider(columns) {
            ui::PINNED_SECTION_GAP
        } else {
            0
        };
        (ui::grid_layer_height(self.grid_len(columns), columns, available_w) as i32 + extra
            - 2 * ui::GRID_LAYER_PAD
            - viewport_h)
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
        let r = self.unscrolled_card_rect(idx, columns, ui::SIDEBAR_W as i32, available_w);
        let viewport_top = ui::GRID_TOP_Y;
        let viewport_bottom = screen_h as i32 - ui::GRID_PAD;
        let max_scroll = self.max_grid_scroll(columns, available_w, screen_h);
        let card_top = r.y() - FOCUS_MARGIN;
        let card_bottom = r.bottom() + FOCUS_MARGIN;
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

    /// Selects host and kicks off async library fetch; avoids blocking the UI thread (used to freeze input).
    pub(crate) fn select_host(&mut self, host: String, port: u16, mgmt_port: Option<u16>) {
        let _ = store::save_selected_host(&host, port);
        self.selected_host = Some((host.clone(), port));
        let name = self
            .known_hosts
            .iter()
            .find(|h| h.host == host && h.port == port)
            .map_or_else(|| host.clone(), |h| h.name.clone());
        self.home_status = Some(format!("Loading library from {name}…"));
        self.games = Vec::new();
        self.pinned_count = 0;
        self.games_loaded = false;
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
        let mgmt_port = mgmt_port.unwrap_or(crate::services::library::DEFAULT_MGMT_PORT);
        tracing::debug!("library: fetching from {host}:{mgmt_port}…");
        self.games_rx = Some(crate::services::library::load_games_async(
            host,
            port,
            mgmt_port,
            identity,
            fingerprint,
        ));
    }

    /// Drains `select_host`'s library fetch; switching hosts aborts old fetches safely.
    pub fn drain_games(&mut self) -> bool {
        let Some(rx) = &self.games_rx else { return false };
        let Ok(loaded) = rx.try_recv() else { return false };
        self.games_rx = None;
        let crate::services::library::GamesLoaded {
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
                let (card_w, card_h) = self.card_size;
                self.art_loader = Some(crate::services::art::ArtLoader::spawn(
                    host,
                    port,
                    mgmt_port,
                    identity,
                    fingerprint,
                    card_w,
                    card_h,
                ));
                self.games = games;
                self.games_loaded = true;
                self.home_status = None;
                self.reorder_games_by_pin();
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
    pub(crate) fn handle_library_error(&mut self, host: String, port: u16, e: crate::services::library::LibraryError) {
        let reason = e.to_string();
        if matches!(e, crate::services::library::LibraryError::Unreachable(_)) {
            let mac = self
                .known_hosts
                .iter()
                .find(|h| h.host == host && h.port == port)
                .map(|h| h.mac.clone())
                .unwrap_or_default();
            self.start_wake(host, port, mac, reason);
        } else {
            // The host answered — just not with a usable library — so Desktop is a
            // legitimate fallback here, unlike the `Unreachable` branch above.
            self.games_loaded = true;
            self.home_status = Some(reason);
        }
    }
    /// Confirms grid card, arming the launch straight away so the connect thread starts on the
    /// click and overlaps the zoom. No pre-flight reachability probe: it cost a whole mTLS
    /// round trip before the handshake, and connect reports an unreachable host itself (the
    /// host list probes on selection, which is where the Wake dialog comes from).
    pub(crate) fn confirm_grid_card(&mut self, idx: usize, columns: usize) {
        if self.launch_ready.is_some() || self.launch_anim.is_some() {
            return;
        }
        let Some((host, port)) = self.selected_host.clone() else {
            return;
        };
        let Some(known) = self.known_hosts.iter().find(|h| h.host == host && h.port == port) else {
            return;
        };
        let Some(fingerprint) = known.fingerprint else { return };
        let (launch, title) = match self.grid_card_at(idx, columns) {
            Some(GridCard::Desktop) => (None, "Desktop".to_string()),
            Some(GridCard::Game(game)) => (Some(game.id.clone()), game.title.clone()),
            None => return,
        };
        // The loading screen's backdrop. Requested here as well as on focus, since a card
        // can be confirmed before the focus prefetch got round to it; Desktop has no art,
        // so it keeps the plain fade to black.
        let mut expected = false;
        if let Some(game) = launch.as_ref().and_then(|id| self.games.iter().find(|g| &g.id == id)) {
            expected = game.art.hero.is_some() || game.art.header.is_some();
            if let Some(loader) = &mut self.art_loader {
                loader.request_hero(game);
            }
        }
        self.hero.arm(launch.clone(), expected);
        tracing::debug!("launch: connecting to {host}:{port} now, zoom runs in parallel");
        // Stays up through the zoom and the connect; `run_inner` owns the screen from there.
        self.home_status = Some(format!("Starting {title}…"));
        self.launch_anim_idx = Some(idx);
        self.launch_ready = Some(ConnectTarget {
            host,
            port,
            fingerprint,
            launch,
        });
        // Not `grid_dirty`: contents are unchanged, and dirtying rebuilds every card tile and
        // re-arms the loading spinner right as the zoom starts.
        self.sidebar_dirty = true;
    }

    /// Takes the `ConnectTarget` `confirm_grid_card` armed, if any — the runtime's tick loop
    /// calls this and breaks its event loop with it to actually start the stream.
    pub fn take_ready_launch(&mut self) -> Option<ConnectTarget> {
        self.launch_ready.take()
    }
    pub(crate) fn forget_host(&mut self, idx: usize) {
        let HostEntry::Known(h) = &self.entries[idx] else {
            return;
        };
        let (host, port) = (h.host.clone(), h.port);
        crate::services::art::clear_host_cache(&host, port);
        self.known_hosts.retain(|k| !(k.host == host && k.port == port));
        let _ = store::save_known_hosts(&self.known_hosts);
        self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
        if self.selected_host.as_ref() == Some(&(host, port)) {
            self.selected_host = None;
            self.games = Vec::new();
            self.games_loaded = false;
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
