//! The pre-stream UI flow: a persistent Home screen (sidebar of known hosts +
//! detail grid of the selected host's games) with Pairing/Settings/Add-host as
//! centered modals on top of it — modeled on moonlight-tv's actual layout (see
//! `ui.rs`'s module docs). `ui.rs` owns drawing/input-mapping primitives,
//! `store.rs` owns persistence, `discovery.rs` owns mDNS.
use std::io::Write as _;

use anyhow::Result;
use sdl2::rect::Rect;
use sdl2::render::Canvas;
use sdl2::video::Window;

use crate::library::GameEntry;
use crate::store::{self, KnownHost, Settings};
use crate::ui::{self, AddHostState, HostEntry, MenuEvent};

pub enum Screen {
    Home,
    Pairing,
    Settings,
    AddHost,
}

/// Which pane of Home currently has focus, and where within it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HomeFocus {
    /// Index into the sidebar: `0..entries.len()` are host rows,
    /// `entries.len()` is "+ Add host", `entries.len() + 1` is "Settings".
    Sidebar(usize),
    /// Index into the grid: `0` is "Desktop", `1..` are `games`.
    Grid(usize),
}

/// What the user picked to stream to, once they confirm a grid card.
pub struct ConnectTarget {
    pub host: String,
    pub port: u16,
    pub fingerprint: [u8; 32],
    /// A library entry's id (`"steam:570"`) to launch into, or `None` for a plain
    /// desktop session — see `crate::library`.
    pub launch: Option<String>,
}

/// An open dropdown on the settings modal (Resolution/Frame rate) — `row` is the
/// settings row index that opened it (`ui::ROW_RESOLUTION`/`ui::ROW_FRAMERATE`),
/// `focused` is the highlighted option within `ui::dropdown_options(row)`.
pub struct DropdownState {
    pub row: usize,
    pub focused: usize,
}

pub struct App {
    pub screen: Screen,
    pub known_hosts: Vec<KnownHost>,
    pub discovered: std::sync::mpsc::Receiver<crate::discovery::DiscoveredHost>,
    pub entries: Vec<HostEntry>,
    pub home_focus: HomeFocus,
    /// The sidebar host whose games are shown in the grid — `None` until one is
    /// selected (or restored from `store::load_selected_host` at startup).
    pub selected_host: Option<(String, u16)>,
    pub games: Vec<GameEntry>,
    /// Library loading/error message shown in the grid area in place of cards.
    pub home_status: Option<String>,
    /// Decoded cover art, keyed by `GameEntry::id` — raw RGBA, not an SDL2 texture
    /// (not `Send`; see `art.rs` docs). `main.rs`'s render loop turns new entries
    /// into textures each tick.
    pub art_pixels: std::collections::HashMap<String, (u32, u32, Vec<u8>)>,
    art_rx: Option<std::sync::mpsc::Receiver<crate::art::ArtLoaded>>,
    pub settings: Settings,
    pub settings_focused: usize,
    pub dropdown: Option<DropdownState>,
    pub add_host: AddHostState,
    /// PIN entry: 4 digits, each 0-9, edited one at a time.
    pub pin_digits: [u8; 4],
    pub pin_digit_index: usize,
    pub pairing_status: Option<String>,
    pub pairing_busy: bool,
    /// Index into `entries` currently being paired — captured when entering
    /// `Screen::Pairing`.
    pairing_entry: usize,
    /// Whether the Magic Remote's pointer is currently hovering a modal's
    /// close (X) button.
    pub hover_close: bool,
    identity: (String, String),
}

impl App {
    pub fn new(identity: (String, String), log: &mut std::fs::File) -> App {
        let known_hosts = store::load_known_hosts();
        let entries = known_hosts.iter().cloned().map(HostEntry::Known).collect();
        let mut app = App {
            screen: Screen::Home,
            known_hosts,
            discovered: crate::discovery::browse(),
            entries,
            home_focus: HomeFocus::Sidebar(0),
            selected_host: None,
            games: Vec::new(),
            home_status: None,
            art_pixels: std::collections::HashMap::new(),
            art_rx: None,
            settings: store::load_settings(),
            settings_focused: 0,
            dropdown: None,
            add_host: AddHostState::default(),
            pin_digits: [0; 4],
            pin_digit_index: 0,
            pairing_status: None,
            pairing_busy: false,
            pairing_entry: 0,
            hover_close: false,
            identity,
        };
        // Restore the last-active sidebar host (if it's still known and paired)
        // so relaunching the app lands back on its game grid.
        if let Some((host, port)) = store::load_selected_host() {
            if let Some(h) = app.known_hosts.iter().find(|h| h.host == host && h.port == port && h.fingerprint.is_some()) {
                let (host, port, mgmt_port) = (h.host.clone(), h.port, h.mgmt_port);
                app.select_host(host, port, mgmt_port, log);
            }
        }
        app
    }

    /// Merges freshly-discovered hosts into the entry list (known hosts keep their
    /// paired status; a discovered host not yet known gets appended).
    pub fn drain_discovery(&mut self) {
        while let Ok(found) = self.discovered.try_recv() {
            let already_known = self
                .known_hosts
                .iter()
                .any(|h| h.host == found.addr && h.port == found.port);
            if !already_known
                && !self
                    .entries
                    .iter()
                    .any(|e| matches!(e, HostEntry::Discovered(d) if d.addr == found.addr && d.port == found.port))
            {
                self.entries.push(HostEntry::Discovered(found));
            }
        }
    }

    /// Drains any cover art that's finished decoding since the last tick — called
    /// alongside `drain_discovery`. Raw pixels only; `main.rs` turns these into
    /// SDL2 textures (see `art.rs`'s module docs for why the split).
    pub fn drain_art(&mut self) {
        let Some(rx) = &self.art_rx else { return };
        while let Ok(loaded) = rx.try_recv() {
            self.art_pixels.insert(loaded.game_id, (loaded.width, loaded.height, loaded.rgba));
        }
    }

    /// Total sidebar nav positions: host rows + "+ Add host" + "Settings".
    fn sidebar_len(&self) -> usize {
        self.entries.len() + 2
    }

    /// Total grid nav positions: "Desktop" + fetched games. `0` (no cards at all)
    /// only when no host is selected yet.
    fn grid_len(&self) -> usize {
        if self.selected_host.is_some() {
            1 + self.games.len()
        } else {
            0
        }
    }

    fn sidebar_index_for_selected(&self) -> usize {
        match &self.selected_host {
            Some((h, p)) => self.entries.iter().position(|e| e.host() == h && e.port() == *p).unwrap_or(0),
            None => 0,
        }
    }

    /// Handles one menu event on the Home screen (sidebar + grid). Returns a
    /// `ConnectTarget` when a grid card is confirmed.
    pub fn handle_home_event(&mut self, ev: MenuEvent, screen_w: u32, log: &mut std::fs::File) -> Option<ConnectTarget> {
        let sidebar_len = self.sidebar_len();
        let grid_len = self.grid_len();
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);

        match ev {
            MenuEvent::Up => match &mut self.home_focus {
                HomeFocus::Sidebar(i) => *i = if *i == 0 { sidebar_len - 1 } else { *i - 1 },
                HomeFocus::Grid(i) => {
                    if *i >= columns {
                        *i -= columns;
                    }
                }
            },
            MenuEvent::Down => match &mut self.home_focus {
                HomeFocus::Sidebar(i) => *i = (*i + 1) % sidebar_len,
                HomeFocus::Grid(i) => {
                    let next = *i + columns;
                    if next < grid_len {
                        *i = next;
                    }
                }
            },
            MenuEvent::Left => {
                if let HomeFocus::Grid(i) = self.home_focus {
                    if i % columns == 0 {
                        self.home_focus = HomeFocus::Sidebar(self.sidebar_index_for_selected());
                    } else {
                        self.home_focus = HomeFocus::Grid(i - 1);
                    }
                }
            }
            MenuEvent::Right => match self.home_focus {
                HomeFocus::Sidebar(_) => {
                    if grid_len > 0 {
                        self.home_focus = HomeFocus::Grid(0);
                    }
                }
                HomeFocus::Grid(i) => {
                    if (i + 1) % columns != 0 && i + 1 < grid_len {
                        self.home_focus = HomeFocus::Grid(i + 1);
                    }
                }
            },
            MenuEvent::Confirm => match self.home_focus {
                HomeFocus::Sidebar(i) if i < self.entries.len() => {
                    self.confirm_sidebar_host(i, log);
                }
                HomeFocus::Sidebar(i) if i == self.entries.len() => {
                    self.add_host = AddHostState::default();
                    self.screen = Screen::AddHost;
                }
                HomeFocus::Sidebar(_) => {
                    self.screen = Screen::Settings;
                    self.dropdown = None;
                    self.settings_focused = 0;
                }
                HomeFocus::Grid(i) => return self.confirm_grid_card(i),
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

    fn confirm_sidebar_host(&mut self, idx: usize, log: &mut std::fs::File) {
        let entry = self.entries[idx].clone();
        match entry {
            HostEntry::Known(h) if h.fingerprint.is_some() => {
                let (host, port, mgmt_port) = (h.host, h.port, h.mgmt_port);
                self.select_host(host, port, mgmt_port, log);
            }
            _ => {
                self.pairing_entry = idx;
                self.pin_digits = [0; 4];
                self.pin_digit_index = 0;
                self.pairing_status = None;
                self.screen = Screen::Pairing;
            }
        }
    }

    /// Makes `(host, port)` the active sidebar selection and (re)fetches its game
    /// library, same "blocking like the PIN ceremony" pattern as pairing — a fetch
    /// failure (host too old, network hiccup) still lands with just "Desktop"
    /// available, since it shouldn't block streaming over a library API quirk.
    fn select_host(&mut self, host: String, port: u16, mgmt_port: Option<u16>, log: &mut std::fs::File) {
        let _ = store::save_selected_host(&host, port);
        self.selected_host = Some((host.clone(), port));
        self.home_status = Some("Loading library…".into());
        self.games = Vec::new();
        self.art_pixels.clear();
        self.art_rx = None;
        self.home_focus = HomeFocus::Grid(0);

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let fingerprint = self.known_hosts.iter().find(|h| h.host == host && h.port == port).and_then(|h| h.fingerprint);
        let mgmt_port = mgmt_port.unwrap_or(crate::library::DEFAULT_MGMT_PORT);
        match crate::library::fetch_games(&host, mgmt_port, &identity, fingerprint) {
            Ok(games) => {
                let _ = writeln!(log, "library: {} games from {host}:{mgmt_port}", games.len());
                self.art_rx = Some(crate::art::load_art_async(host, mgmt_port, identity, fingerprint, games.clone()));
                self.games = games;
                self.home_status = None;
            }
            Err(e) => {
                let _ = writeln!(log, "library fetch failed ({host}:{mgmt_port}): {e}");
                self.home_status = Some(format!("{e} (Desktop is still available.)"));
            }
        }
    }

    fn confirm_grid_card(&self, idx: usize) -> Option<ConnectTarget> {
        let (host, port) = self.selected_host.clone()?;
        let fingerprint = self.known_hosts.iter().find(|h| h.host == host && h.port == port)?.fingerprint?;
        let launch = if idx == 0 { None } else { Some(self.games.get(idx - 1)?.id.clone()) };
        Some(ConnectTarget { host, port, fingerprint, launch })
    }

    fn forget_host(&mut self, idx: usize) {
        let HostEntry::Known(h) = &self.entries[idx] else { return };
        let (host, port) = (h.host.clone(), h.port);
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
    }

    /// Handles one menu event on the pairing modal. Runs the (blocking) PIN
    /// pairing ceremony on `Confirm` over the "Pair" action.
    pub fn handle_pairing_event(&mut self, ev: MenuEvent, log: &mut std::fs::File) {
        if self.pairing_busy {
            return; // ignore input mid-ceremony
        }
        match ev {
            MenuEvent::Left => {
                self.pin_digits[self.pin_digit_index] = (self.pin_digits[self.pin_digit_index] + 9) % 10;
            }
            MenuEvent::Right => {
                self.pin_digits[self.pin_digit_index] = (self.pin_digits[self.pin_digit_index] + 1) % 10;
            }
            MenuEvent::Up => {
                if self.pin_digit_index > 0 {
                    self.pin_digit_index -= 1;
                }
            }
            MenuEvent::Down => {
                if self.pin_digit_index + 1 < self.pin_digits.len() {
                    self.pin_digit_index += 1;
                } else {
                    self.try_pair(log);
                }
            }
            MenuEvent::Confirm => self.try_pair(log),
            MenuEvent::Back => self.screen = Screen::Home,
            MenuEvent::Secondary => {}
        }
    }

    /// Direct digit entry (the Magic Remote's number buttons) — types `digit` into
    /// the current PIN slot and auto-advances, like a phone lock-screen PIN pad,
    /// instead of requiring left/right cycling through 0-9 per digit.
    pub fn enter_pin_digit(&mut self, digit: u8, log: &mut std::fs::File) {
        if self.pairing_busy {
            return;
        }
        self.pin_digits[self.pin_digit_index] = digit;
        if self.pin_digit_index + 1 < self.pin_digits.len() {
            self.pin_digit_index += 1;
        } else {
            self.try_pair(log);
        }
    }

    fn try_pair(&mut self, log: &mut std::fs::File) {
        let entry = &self.entries[self.pairing_entry];
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        let mgmt_port = entry.mgmt_port();
        let pin: String = self.pin_digits.iter().map(|d| d.to_string()).collect();
        self.pairing_busy = true;
        self.pairing_status = Some("Pairing… confirm the PIN on the host".into());
        let _ = writeln!(log, "pairing with {host}:{port} (pin len {})", pin.len());

        let identity = (self.identity.0.as_str(), self.identity.1.as_str());
        match punktfunk_core::client::NativeClient::pair(
            &host,
            port,
            identity,
            &pin,
            "webOS TV",
            std::time::Duration::from_secs(30),
        ) {
            Ok(fingerprint) => {
                let _ = writeln!(log, "paired ok, fingerprint set");
                let paired_host = host.clone();
                store::upsert_known_host(
                    &mut self.known_hosts,
                    KnownHost { name, host, port, fingerprint: Some(fingerprint), mgmt_port },
                );
                let _ = store::save_known_hosts(&self.known_hosts);
                self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
                self.screen = Screen::Home;
                self.select_host(paired_host, port, mgmt_port, log);
            }
            Err(e) => {
                let _ = writeln!(log, "pairing failed: {e:#}");
                self.pairing_status = Some(format!("Pairing failed: {e}"));
            }
        }
        self.pairing_busy = false;
    }

    /// Handles one menu event on the settings modal.
    pub fn handle_settings_event(&mut self, ev: MenuEvent) {
        // An open Resolution/Frame rate dropdown intercepts all input until it's
        // closed (by picking an option or backing out) — it's a modal overlay on
        // top of the settings row list.
        if let Some(dd) = self.dropdown.as_mut() {
            let row = dd.row;
            let len = ui::dropdown_options(row).len().max(1);
            match ev {
                MenuEvent::Up => dd.focused = if dd.focused == 0 { len - 1 } else { dd.focused - 1 },
                MenuEvent::Down => dd.focused = (dd.focused + 1) % len,
                MenuEvent::Confirm => {
                    let choice = dd.focused;
                    ui::apply_dropdown_choice(&mut self.settings, row, choice);
                    let _ = store::save_settings(&self.settings);
                    self.dropdown = None;
                }
                MenuEvent::Back => self.dropdown = None,
                MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => {}
            }
            return;
        }
        let total = ui::SETTINGS_ROW_COUNT;
        match ev {
            MenuEvent::Up => {
                self.settings_focused = if self.settings_focused == 0 { total - 1 } else { self.settings_focused - 1 };
            }
            MenuEvent::Down => {
                self.settings_focused = (self.settings_focused + 1) % total;
            }
            MenuEvent::Left => {
                if ui::adjust_setting(&mut self.settings, self.settings_focused, false) {
                    let _ = store::save_settings(&self.settings);
                }
            }
            MenuEvent::Right => {
                if ui::adjust_setting(&mut self.settings, self.settings_focused, true) {
                    let _ = store::save_settings(&self.settings);
                }
            }
            MenuEvent::Confirm => match self.settings_focused {
                ui::ROW_RESOLUTION | ui::ROW_FRAMERATE => {
                    let focused = ui::dropdown_current_index(&self.settings, self.settings_focused);
                    self.dropdown = Some(DropdownState { row: self.settings_focused, focused });
                }
                row => {
                    if ui::adjust_setting(&mut self.settings, row, true) {
                        let _ = store::save_settings(&self.settings);
                    }
                }
            },
            MenuEvent::Back => self.screen = Screen::Home,
            MenuEvent::Secondary => {}
        }
    }

    /// Handles one menu event on the manual add-host modal (17 digit slots: four
    /// 3-digit IP octets + a 5-digit port — see `ui::AddHostState`).
    pub fn handle_add_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => {
                let d = &mut self.add_host.digits[self.add_host.index];
                *d = (*d + 9) % 10;
                self.add_host.touch_current();
            }
            MenuEvent::Right => {
                let d = &mut self.add_host.digits[self.add_host.index];
                *d = (*d + 1) % 10;
                self.add_host.touch_current();
            }
            MenuEvent::Up => {
                if self.add_host.index > 0 {
                    self.add_host.index -= 1;
                }
            }
            MenuEvent::Down => {
                if self.add_host.index + 1 < self.add_host.digits.len() {
                    self.add_host.index += 1;
                } else {
                    self.confirm_add_host();
                }
            }
            MenuEvent::Confirm => self.confirm_add_host(),
            MenuEvent::Back => self.screen = Screen::Home,
            MenuEvent::Secondary => {}
        }
    }

    /// Direct digit entry (the Magic Remote's number buttons) on the add-host
    /// modal — same auto-advance idiom as `enter_pin_digit`.
    pub fn enter_add_host_digit(&mut self, digit: u8) {
        self.add_host.digits[self.add_host.index] = digit;
        self.add_host.touch_current();
        if self.add_host.index + 1 < self.add_host.digits.len() {
            self.add_host.index += 1;
        } else {
            self.confirm_add_host();
        }
    }

    fn confirm_add_host(&mut self) {
        let (host, port) = self.add_host.host_and_port();
        store::upsert_known_host(
            &mut self.known_hosts,
            KnownHost { name: host.clone(), host: host.clone(), port, fingerprint: None, mgmt_port: None },
        );
        let _ = store::save_known_hosts(&self.known_hosts);
        self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
        self.home_focus = HomeFocus::Sidebar(self.entries.iter().position(|e| e.host() == host && e.port() == port).unwrap_or(0));
        self.screen = Screen::Home;
    }

    // ---------------------------------------------------------------- mouse --

    /// The pairing modal's card rect — shared by `render_pairing` and mouse
    /// hit-testing so they can never disagree.
    fn pairing_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::modal_card_rect(screen_w, screen_h, 0.36, 340)
    }

    /// The add-host modal's card rect — shared by `render_add_host` and mouse
    /// hit-testing.
    fn add_host_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::modal_card_rect(screen_w, screen_h, 0.46, 260)
    }

    /// The settings modal's card/content rects — shared by `render` and mouse
    /// hit-testing so they can never disagree.
    fn settings_layout(screen_w: u32, screen_h: u32) -> (Rect, Rect) {
        let content_h = ui::SETTINGS_ROW_COUNT as u32 * (ui::SETTINGS_ROW_H + ui::SETTINGS_ROW_GAP as u32);
        // Room for the title/divider above and the high-bitrate caution below.
        let card_h = content_h + 200;
        let card = ui::modal_card_rect(screen_w, screen_h, 0.56, card_h);
        let content = Rect::new(card.x() + 40, card.y() + 120, card.width().saturating_sub(80), content_h);
        (card, content)
    }

    /// Updates focus/hover to whatever the Magic Remote's pointer is over.
    pub fn handle_mouse_motion(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) {
        match self.screen {
            Screen::Home => {
                if let Some(idx) = ui::hit_test_sidebar_row(x, y, self.sidebar_len()) {
                    self.home_focus = HomeFocus::Sidebar(idx);
                    return;
                }
                let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
                let columns = ui::grid_columns(available_w);
                if let Some(idx) = ui::hit_test_grid_card(x, y, columns, self.grid_len(), ui::SIDEBAR_W as i32, available_w) {
                    self.home_focus = HomeFocus::Grid(idx);
                }
            }
            Screen::Settings => {
                let (card, content) = Self::settings_layout(screen_w, screen_h);
                self.hover_close = ui::modal_close_rect(card).contains_point((x, y));
                if self.dropdown.is_none() && !self.hover_close {
                    for i in 0..ui::SETTINGS_ROW_COUNT {
                        let row_y = content.y() + i as i32 * (ui::SETTINGS_ROW_H as i32 + ui::SETTINGS_ROW_GAP);
                        let row_rect = Rect::new(content.x(), row_y, content.width(), ui::SETTINGS_ROW_H);
                        if row_rect.contains_point((x, y)) {
                            self.settings_focused = i;
                            break;
                        }
                    }
                }
            }
            Screen::Pairing => {
                let card = Self::pairing_card_rect(screen_w, screen_h);
                self.hover_close = ui::modal_close_rect(card).contains_point((x, y));
            }
            Screen::AddHost => {
                let card = Self::add_host_card_rect(screen_w, screen_h);
                self.hover_close = ui::modal_close_rect(card).contains_point((x, y));
            }
        }
    }

    /// A pointer click confirms whatever's currently hovered/focused, or triggers
    /// Back if the modal's close (X) button itself is what's hovered.
    pub fn handle_mouse_click(&mut self, log: &mut std::fs::File) -> Option<ConnectTarget> {
        if self.hover_close {
            match self.screen {
                Screen::Settings => self.handle_settings_event(MenuEvent::Back),
                Screen::Pairing => self.handle_pairing_event(MenuEvent::Back, log),
                Screen::AddHost => self.handle_add_host_event(MenuEvent::Back),
                Screen::Home => {}
            }
            return None;
        }
        match self.screen {
            // screen_w isn't known here — mouse clicks confirm whatever
            // `handle_mouse_motion` already focused, so the grid-column math
            // `handle_home_event` needs isn't actually exercised by a Confirm.
            Screen::Home => self.handle_home_event(MenuEvent::Confirm, u32::MAX, log),
            Screen::Settings => {
                self.handle_settings_event(MenuEvent::Confirm);
                None
            }
            Screen::Pairing | Screen::AddHost => None,
        }
    }

    // --------------------------------------------------------------- render --

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        canvas: &mut Canvas<Window>,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        art: &std::collections::HashMap<String, sdl2::render::Texture>,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        canvas.set_draw_color(ui::BG);
        canvas.clear();
        self.render_home(canvas, texture_creator, font_label, font_value, font_title, art, screen_w, screen_h)?;

        match self.screen {
            Screen::Home => {}
            Screen::Pairing => self.render_pairing(canvas, texture_creator, font_label, font_title, screen_w, screen_h)?,
            Screen::Settings => self.render_settings(canvas, texture_creator, font_label, font_value, screen_w, screen_h)?,
            Screen::AddHost => self.render_add_host(canvas, texture_creator, font_label, font_value, font_title, screen_w, screen_h)?,
        }
        canvas.present();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_home(
        &self,
        canvas: &mut Canvas<Window>,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        art: &std::collections::HashMap<String, sdl2::render::Texture>,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let sidebar_focus = match self.home_focus {
            HomeFocus::Sidebar(i) => Some(i),
            HomeFocus::Grid(_) => None,
        };
        ui::draw_sidebar(canvas, texture_creator, font_label, font_title, &self.entries, sidebar_focus, screen_h)?;

        let grid_x = ui::SIDEBAR_W as i32;
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        if self.selected_host.is_none() {
            ui::draw_text(
                canvas,
                texture_creator,
                font_label,
                "No host selected — pick one from the list, or add one.",
                grid_x + ui::GRID_PAD,
                ui::GRID_TOP_Y,
                ui::MUTED,
            )?;
            return Ok(());
        }
        if let Some(status) = &self.home_status {
            ui::draw_text(canvas, texture_creator, font_label, status, grid_x + ui::GRID_PAD, ui::GRID_TOP_Y, ui::MUTED)?;
        }
        let columns = ui::grid_columns(available_w);
        let grid_focus = match self.home_focus {
            HomeFocus::Grid(i) => Some(i),
            HomeFocus::Sidebar(_) => None,
        };
        // Card 0 is always "Desktop" (a plain session, no game launch) — never has
        // fetched art of its own.
        let desktop_rect = ui::grid_card_rect(0, columns, grid_x, available_w);
        ui::draw_poster_card(canvas, texture_creator, font_title, font_value, desktop_rect, "Desktop", None, grid_focus == Some(0))?;
        for (i, game) in self.games.iter().enumerate() {
            let idx = i + 1;
            let rect = ui::grid_card_rect(idx, columns, grid_x, available_w);
            ui::draw_poster_card(canvas, texture_creator, font_title, font_value, rect, &game.title, art.get(&game.id), grid_focus == Some(idx))?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_pairing(
        &self,
        canvas: &mut Canvas<Window>,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        font_label: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        ui::draw_modal_backdrop(canvas, screen_w, screen_h);
        let card = Self::pairing_card_rect(screen_w, screen_h);
        ui::draw_modal_card(canvas, card);
        let close_rect = ui::modal_close_rect(card);
        ui::draw_close_icon(canvas, close_rect, if self.hover_close { ui::WHITE } else { ui::MUTED });

        ui::draw_text(canvas, texture_creator, font_title, "Pair with host", card.x() + 32, card.y() + 28, ui::WHITE)?;
        ui::draw_text(
            canvas,
            texture_creator,
            font_label,
            "Enter the PIN shown in the host's pairing dialog.",
            card.x() + 32,
            card.y() + 84,
            ui::MUTED,
        )?;

        let digit_w = 64i32;
        let digit_gap = 14i32;
        let total_w = 4 * digit_w + 3 * digit_gap;
        let start_x = card.x() + (card.width() as i32 - total_w) / 2;
        let digit_y = card.y() + 130;
        for (i, digit) in self.pin_digits.iter().enumerate() {
            let x = start_x + i as i32 * (digit_w + digit_gap);
            let rect = Rect::new(x, digit_y, digit_w as u32, 80);
            let focused = i == self.pin_digit_index;
            let drawn = ui::draw_card(canvas, rect, focused);
            let text = digit.to_string();
            let tw = font_title.size_of(&text).map(|(w, _)| w).unwrap_or(0);
            ui::draw_text(
                canvas,
                texture_creator,
                font_title,
                &text,
                drawn.x() + (drawn.width() as i32 - tw as i32) / 2,
                drawn.y() + (drawn.height() as i32 - font_title.height()) / 2,
                ui::WHITE,
            )?;
        }
        if let Some(status) = &self.pairing_status {
            let color = if self.pairing_busy { ui::MUTED } else { ui::ERROR_RED };
            ui::draw_text(canvas, texture_creator, font_label, status, card.x() + 32, digit_y + 100, color)?;
        }
        Ok(())
    }

    fn render_settings(
        &self,
        canvas: &mut Canvas<Window>,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        ui::draw_modal_backdrop(canvas, screen_w, screen_h);
        let (card, content) = Self::settings_layout(screen_w, screen_h);
        ui::draw_modal_card(canvas, card);
        let close_rect = ui::modal_close_rect(card);
        ui::draw_close_icon(canvas, close_rect, if self.hover_close { ui::WHITE } else { ui::MUTED });
        ui::draw_text(canvas, texture_creator, font_label, "Settings", card.x() + 40, card.y() + 36, ui::WHITE)?;
        canvas.set_draw_color(sdl2::pixels::Color::RGBA(0xff, 0xff, 0xff, 0x1e));
        let _ = canvas.fill_rect(Rect::new(card.x() + 40, card.y() + 88, card.width().saturating_sub(80), 1));

        let rows = ui::settings_rows(&self.settings);
        ui::draw_settings_rows(canvas, texture_creator, font_label, font_value, &rows, self.settings_focused, content)?;

        if self.settings.bitrate_kbps > ui::BITRATE_WARN_KBPS {
            ui::draw_text(
                canvas,
                texture_creator,
                font_value,
                "Higher bitrate may be unstable on Wi-Fi — try Ethernet if streaming drops.",
                content.x(),
                content.y() + content.height() as i32 + 16,
                ui::WARNING,
            )?;
        }

        if let Some(dd) = &self.dropdown {
            let options = ui::dropdown_options(dd.row);
            let overlay_y = content.y() + (dd.row as i32 + 1) * (ui::SETTINGS_ROW_H as i32 + ui::SETTINGS_ROW_GAP);
            let overlay_rect = Rect::new(content.x(), overlay_y, content.width(), 0);
            ui::draw_dropdown_overlay(canvas, texture_creator, font_value, &options, dd.focused, overlay_rect)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_add_host(
        &self,
        canvas: &mut Canvas<Window>,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        ui::draw_modal_backdrop(canvas, screen_w, screen_h);
        let card = Self::add_host_card_rect(screen_w, screen_h);
        ui::draw_modal_card(canvas, card);
        let close_rect = ui::modal_close_rect(card);
        ui::draw_close_icon(canvas, close_rect, if self.hover_close { ui::WHITE } else { ui::MUTED });

        ui::draw_text(canvas, texture_creator, font_label, "Add host", card.x() + 32, card.y() + 28, ui::WHITE)?;
        ui::draw_text(
            canvas,
            texture_creator,
            font_value,
            "Enter the host's IP address and port.",
            card.x() + 32,
            card.y() + 74,
            ui::MUTED,
        )?;

        let text = self.add_host.display_text();
        let focus_char = self.add_host.focus_char_index();
        let field = Rect::new(card.x() + 32, card.y() + 120, card.width().saturating_sub(64), 80);
        let drawn = ui::draw_card(canvas, field, true);
        let text_w = font_title.size_of(&text).map(|(w, _)| w).unwrap_or(0);
        ui::draw_highlighted_text(
            canvas,
            texture_creator,
            font_title,
            &text,
            focus_char,
            drawn.x() + (drawn.width() as i32 - text_w as i32) / 2,
            drawn.y() + (drawn.height() as i32 - font_title.height()) / 2,
            ui::WHITE,
            ui::ACCENT_BRIGHT,
        )?;
        Ok(())
    }
}
