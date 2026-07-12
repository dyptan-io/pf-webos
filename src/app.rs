//! The pre-stream UI flow: host list → PIN pairing (if needed) → settings (optional)
//! → connect. Owns the navigation state machine; `ui.rs` owns drawing/input-mapping
//! primitives, `store.rs` owns persistence, `discovery.rs` owns mDNS.
use std::io::Write as _;

use anyhow::Result;
use sdl2::rect::Rect;
use sdl2::render::Canvas;
use sdl2::video::Window;

use crate::library::GameEntry;
use crate::store::{self, KnownHost, Settings};
use crate::ui::{self, AddHostState, HostEntry, MenuEvent, Row};

pub enum Screen {
    HostList,
    Pairing,
    Settings,
    AddHost,
    /// Shown after confirming an already-paired host, before actually connecting —
    /// "Desktop" plus whatever the host's library API returns (see `enter_library`).
    Library,
}

/// What the user picked to stream to, once they confirm a paired host.
pub struct ConnectTarget {
    pub host: String,
    pub port: u16,
    pub fingerprint: [u8; 32],
    /// A library entry's id (`"steam:570"`) to launch into, or `None` for a plain
    /// desktop session — see `crate::library`.
    pub launch: Option<String>,
}

/// An open dropdown on the settings screen (Resolution/Frame rate) — `row` is the
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
    /// Navigation index across the host-list screen's focusable spots: `0` is the
    /// header's Settings button, `1..=entries.len()` are host rows, and
    /// `entries.len() + 1` is the trailing "+ Add host" row — see `host_rows`.
    pub focused: usize,
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
    /// `Screen::Pairing`, since `focused` (a nav-index into the host-list, not the
    /// entries list) isn't usable once we've left that screen.
    pairing_entry: usize,
    /// The host being connected to, captured when entering `Screen::Library` — the
    /// eventual `ConnectTarget` is built from this plus whichever library row gets
    /// confirmed.
    library_host: Option<ConnectTarget>,
    pub games: Vec<GameEntry>,
    pub library_focused: usize,
    pub library_status: Option<String>,
    /// Whether the Magic Remote's pointer is currently hovering the persistent
    /// top-left Back button — checked first by `handle_mouse_click`, since Back
    /// isn't a normal focusable row on every screen (see `handle_mouse_motion`).
    pub hover_back: bool,
    identity: (String, String),
}

impl App {
    pub fn new(identity: (String, String)) -> App {
        let known_hosts = store::load_known_hosts();
        let entries = known_hosts.iter().cloned().map(HostEntry::Known).collect();
        App {
            screen: Screen::HostList,
            known_hosts,
            discovered: crate::discovery::browse(),
            entries,
            focused: 0,
            settings: store::load_settings(),
            settings_focused: 0,
            dropdown: None,
            add_host: AddHostState::default(),
            pin_digits: [0; 4],
            pin_digit_index: 0,
            pairing_status: None,
            pairing_busy: false,
            pairing_entry: 0,
            library_host: None,
            games: Vec::new(),
            library_focused: 0,
            library_status: None,
            hover_back: false,
            identity,
        }
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

    /// The host entries plus a trailing "+ Add host" row. The Settings button lives
    /// separately in the header (see `ui::settings_button_rect`) — deliberately not
    /// mixed into this list (it used to be a synthetic trailing row here, which was
    /// both the wrong place for it and rendered with a glyph the system font lacks).
    fn host_rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .entries
            .iter()
            .map(|e| Row {
                label: e.name().to_string(),
                value: if e.is_paired() {
                    format!("{}:{} (paired)", e.host(), e.port())
                } else {
                    format!("{}:{} (not paired)", e.host(), e.port())
                },
                kind: ui::RowKind::Action,
                fraction: 0.0,
            })
            .collect();
        rows.push(Row::action("+ Add host", ""));
        rows
    }

    /// Total keyboard/remote-navigable spots on the host-list screen: the header
    /// Settings button (nav index 0), each host row (`1..=entries.len()`), and the
    /// trailing "+ Add host" row (`entries.len() + 1`).
    fn total_nav_positions(&self) -> usize {
        self.entries.len() + 2
    }

    /// Handles one menu event on the host-list screen. Confirming an already-paired
    /// host doesn't connect directly — it opens `Screen::Library` first (see
    /// `enter_library`), which is where a `ConnectTarget` eventually comes from.
    pub fn handle_host_list_event(&mut self, ev: MenuEvent, log: &mut std::fs::File) {
        let total = self.total_nav_positions();
        match ev {
            MenuEvent::Up => {
                self.focused = if self.focused == 0 { total - 1 } else { self.focused - 1 };
            }
            MenuEvent::Down => {
                self.focused = (self.focused + 1) % total;
            }
            MenuEvent::Confirm => {
                if self.focused == 0 {
                    self.screen = Screen::Settings;
                    self.dropdown = None;
                    return;
                }
                if self.focused == self.entries.len() + 1 {
                    self.add_host = AddHostState::default();
                    self.screen = Screen::AddHost;
                    return;
                }
                let idx = self.focused - 1;
                let entry = self.entries[idx].clone();
                match entry {
                    HostEntry::Known(h) if h.fingerprint.is_some() => {
                        let fingerprint = h.fingerprint.unwrap();
                        self.enter_library(h.host, h.port, fingerprint, h.mgmt_port, log);
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
            // Forgets the focused host (removes its persisted entry/fingerprint —
            // it'll reappear as "not paired" if still discoverable on the LAN).
            // No-op on the Settings button or the "+ Add host" row.
            MenuEvent::Secondary => {
                if self.focused >= 1 && self.focused <= self.entries.len() {
                    let idx = self.focused - 1;
                    if let HostEntry::Known(h) = &self.entries[idx] {
                        let (host, port) = (h.host.clone(), h.port);
                        self.known_hosts.retain(|k| !(k.host == host && k.port == port));
                        let _ = store::save_known_hosts(&self.known_hosts);
                        self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
                        let total = self.total_nav_positions();
                        if self.focused >= total {
                            self.focused = total - 1;
                        }
                    }
                }
            }
            MenuEvent::Left | MenuEvent::Right | MenuEvent::Back => {}
        }
    }

    /// Enters the library screen for a chosen paired host: fetches its game list
    /// (blocking, like `try_pair`'s PIN ceremony) and stashes the host/port/
    /// fingerprint so `handle_library_event` can build the final `ConnectTarget`
    /// once the user picks "Desktop" or a game. A fetch failure (host too old, not
    /// actually paired, network hiccup) still lands on the library screen with just
    /// "Desktop" available — it shouldn't block streaming over a library API quirk.
    fn enter_library(&mut self, host: String, port: u16, fingerprint: [u8; 32], mgmt_port: Option<u16>, log: &mut std::fs::File) {
        self.library_status = Some("Loading library…".into());
        self.games = Vec::new();
        self.library_focused = 1; // land on "Desktop", not the Back button at 0
        self.screen = Screen::Library;
        self.library_host = Some(ConnectTarget { host: host.clone(), port, fingerprint, launch: None });

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let mgmt_port = mgmt_port.unwrap_or(crate::library::DEFAULT_MGMT_PORT);
        match crate::library::fetch_games(&host, mgmt_port, &identity, Some(fingerprint)) {
            Ok(games) => {
                let _ = writeln!(log, "library: {} games from {host}:{mgmt_port}", games.len());
                self.games = games;
                self.library_status = None;
            }
            Err(e) => {
                let _ = writeln!(log, "library fetch failed ({host}:{mgmt_port}): {e}");
                self.library_status = Some(format!("{e} (Desktop is still available.)"));
            }
        }
    }

    /// Handles one menu event on the library screen. Nav index 0 is the top-left
    /// Back button, 1 is always "Desktop" (`launch: None`), and indices 2.. are the
    /// fetched `games`, in order — same "utility slot before the real list" pattern
    /// as the host-list screen's header Settings button.
    pub fn handle_library_event(&mut self, ev: MenuEvent) -> Option<ConnectTarget> {
        let total = 2 + self.games.len();
        match ev {
            MenuEvent::Up => {
                self.library_focused =
                    if self.library_focused == 0 { total - 1 } else { self.library_focused - 1 };
                None
            }
            MenuEvent::Down => {
                self.library_focused = (self.library_focused + 1) % total;
                None
            }
            MenuEvent::Confirm => {
                if self.library_focused == 0 {
                    self.screen = Screen::HostList;
                    self.library_host = None;
                    return None;
                }
                let base = self.library_host.as_ref()?;
                let launch =
                    if self.library_focused == 1 { None } else { Some(self.games[self.library_focused - 2].id.clone()) };
                Some(ConnectTarget { host: base.host.clone(), port: base.port, fingerprint: base.fingerprint, launch })
            }
            MenuEvent::Back => {
                self.screen = Screen::HostList;
                self.library_host = None;
                None
            }
            MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => None,
        }
    }

    /// Handles one menu event on the pairing screen. Runs the (blocking) PIN
    /// pairing ceremony on `Confirm` over the "Pair" action.
    pub fn handle_pairing_event(&mut self, ev: MenuEvent, log: &mut std::fs::File) {
        if self.pairing_busy {
            return; // ignore input mid-ceremony
        }
        match ev {
            MenuEvent::Left => {
                self.pin_digits[self.pin_digit_index] =
                    (self.pin_digits[self.pin_digit_index] + 9) % 10;
            }
            MenuEvent::Right => {
                self.pin_digits[self.pin_digit_index] =
                    (self.pin_digits[self.pin_digit_index] + 1) % 10;
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
            MenuEvent::Back => self.screen = Screen::HostList,
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

    /// Row count for the current screen's list, if it has one — shared by the
    /// Magic Remote pointer's hover hit-testing (`ui::hit_test_row`). The host-list's
    /// header Settings button is hit-tested separately (see `handle_mouse_motion`).
    fn current_row_count(&self) -> usize {
        match self.screen {
            Screen::HostList => self.entries.len() + 1, // + "Add host" row
            Screen::Settings => ui::SETTINGS_ROW_COUNT,
            Screen::Library => 1 + self.games.len(), // "Desktop" + games
            Screen::Pairing | Screen::AddHost => 0,
        }
    }

    /// Updates focus to whatever row the Magic Remote's pointer is hovering. Every
    /// non-root screen has a persistent top-left Back button
    /// (`ui::back_button_rect`), checked first and shared across all of them —
    /// hovering it takes priority over any row underneath (there isn't one, since
    /// rows start well below it).
    pub fn handle_mouse_motion(&mut self, x: i32, y: i32, screen_w: u32) {
        self.hover_back = !matches!(self.screen, Screen::HostList) && ui::hit_test_back_button(x, y);
        if self.hover_back {
            match self.screen {
                Screen::Settings => self.settings_focused = 0,
                Screen::Library => self.library_focused = 0,
                _ => {}
            }
            return;
        }
        match self.screen {
            Screen::HostList => {
                if ui::settings_button_rect(screen_w).contains_point((x, y)) {
                    self.focused = 0;
                } else if let Some(idx) = ui::hit_test_row(x, y, screen_w, self.current_row_count()) {
                    self.focused = idx + 1;
                }
            }
            Screen::Settings => {
                if self.dropdown.is_none() {
                    if let Some(idx) = ui::hit_test_row(x, y, screen_w, self.current_row_count()) {
                        self.settings_focused = idx + 1; // +1: nav index 0 is Back
                    }
                }
            }
            Screen::Library => {
                if let Some(idx) = ui::hit_test_row(x, y, screen_w, self.current_row_count()) {
                    self.library_focused = idx + 2; // +2: 0 = Back, 1 = Desktop
                }
            }
            Screen::Pairing | Screen::AddHost => {}
        }
    }

    /// A pointer click confirms whatever row is currently hovered/focused (mouse
    /// motion already updated focus — see `handle_mouse_motion`), or triggers Back
    /// directly if the Back button itself is what's hovered.
    pub fn handle_mouse_click(&mut self, log: &mut std::fs::File) -> Option<ConnectTarget> {
        if self.hover_back {
            return match self.screen {
                Screen::Settings => {
                    self.handle_settings_event(MenuEvent::Confirm);
                    None
                }
                Screen::Library => self.handle_library_event(MenuEvent::Confirm),
                Screen::Pairing => {
                    self.handle_pairing_event(MenuEvent::Back, log);
                    None
                }
                Screen::AddHost => {
                    self.handle_add_host_event(MenuEvent::Back);
                    None
                }
                Screen::HostList => None,
            };
        }
        match self.screen {
            Screen::HostList => {
                self.handle_host_list_event(MenuEvent::Confirm, log);
                None
            }
            Screen::Settings => {
                self.handle_settings_event(MenuEvent::Confirm);
                None
            }
            Screen::Library => self.handle_library_event(MenuEvent::Confirm),
            Screen::Pairing | Screen::AddHost => None,
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
                    KnownHost {
                        name,
                        host,
                        port,
                        fingerprint: Some(fingerprint),
                        mgmt_port,
                    },
                );
                let _ = store::save_known_hosts(&self.known_hosts);
                self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
                // Focus the freshly-paired host's row (nav index = list index + 1,
                // since nav index 0 is the header Settings button).
                self.focused = self
                    .entries
                    .iter()
                    .position(|e| e.host() == paired_host && e.port() == port)
                    .map(|i| i + 1)
                    .unwrap_or(1);
                self.screen = Screen::HostList;
            }
            Err(e) => {
                let _ = writeln!(log, "pairing failed: {e:#}");
                self.pairing_status = Some(format!("Pairing failed: {e}"));
            }
        }
        self.pairing_busy = false;
    }

    /// Handles one menu event on the settings screen. Nav index 0 is the top-left
    /// Back button (`ui::back_button_rect`); indices `1..=ui::SETTINGS_ROW_COUNT`
    /// map to `ui::settings_rows()` at `index - 1` — reachable by Confirm as well
    /// as the `Back` keybinding itself, since not every remote's Back button maps
    /// to a keycode this app watches for (the settings-screen equivalent of why
    /// the Settings button itself needed a dedicated header spot instead of
    /// relying on Back from the host list).
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
        let total = 1 + ui::SETTINGS_ROW_COUNT;
        match ev {
            MenuEvent::Up => {
                self.settings_focused = if self.settings_focused == 0 { total - 1 } else { self.settings_focused - 1 };
            }
            MenuEvent::Down => {
                self.settings_focused = (self.settings_focused + 1) % total;
            }
            MenuEvent::Left => {
                if self.settings_focused > 0
                    && ui::adjust_setting(&mut self.settings, self.settings_focused - 1, false)
                {
                    let _ = store::save_settings(&self.settings);
                }
            }
            MenuEvent::Right => {
                if self.settings_focused > 0
                    && ui::adjust_setting(&mut self.settings, self.settings_focused - 1, true)
                {
                    let _ = store::save_settings(&self.settings);
                }
            }
            MenuEvent::Confirm => {
                if self.settings_focused == 0 {
                    self.screen = Screen::HostList;
                    return;
                }
                let row = self.settings_focused - 1;
                match row {
                    ui::ROW_RESOLUTION | ui::ROW_FRAMERATE => {
                        let focused = ui::dropdown_current_index(&self.settings, row);
                        self.dropdown = Some(DropdownState { row, focused });
                    }
                    _ => {
                        if ui::adjust_setting(&mut self.settings, row, true) {
                            let _ = store::save_settings(&self.settings);
                        }
                    }
                }
            }
            MenuEvent::Back => self.screen = Screen::HostList,
            MenuEvent::Secondary => {}
        }
    }

    /// Handles one menu event on the manual add-host screen (17 digit slots: four
    /// 3-digit IP octets + a 5-digit port — see `ui::AddHostState`).
    pub fn handle_add_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => {
                let d = &mut self.add_host.digits[self.add_host.index];
                *d = (*d + 9) % 10;
            }
            MenuEvent::Right => {
                let d = &mut self.add_host.digits[self.add_host.index];
                *d = (*d + 1) % 10;
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
            MenuEvent::Back => self.screen = Screen::HostList,
            MenuEvent::Secondary => {}
        }
    }

    /// Direct digit entry (the Magic Remote's number buttons) on the add-host
    /// screen — same auto-advance idiom as `enter_pin_digit`.
    pub fn enter_add_host_digit(&mut self, digit: u8) {
        self.add_host.digits[self.add_host.index] = digit;
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
        self.focused = self
            .entries
            .iter()
            .position(|e| e.host() == host && e.port() == port)
            .map(|i| i + 1)
            .unwrap_or(1);
        self.screen = Screen::HostList;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        canvas: &mut Canvas<Window>,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        canvas.set_draw_color(ui::FORM_BG);
        canvas.clear();
        ui::fill_vertical_gradient(canvas, screen_w, screen_h, ui::FORM_BG_TOP, ui::FORM_BG);
        let left = ((screen_w.saturating_sub(ui::ROW_MAX_W)) / 2) as i32;
        let width = ui::ROW_MAX_W.min(screen_w.saturating_sub(64));

        match self.screen {
            Screen::HostList => {
                ui::draw_text(canvas, texture_creator, font_title, "punktfunk", left, 48, ui::WHITE)?;
                // The Settings button lives in the header, separate from the host
                // rows below — a vector-drawn gear icon, not a font glyph (the
                // system font has no gear/U+2699, and plain "Settings" text read
                // as less TV-native — see `ui::draw_gear_icon` docs).
                let settings_rect = ui::settings_button_rect(screen_w);
                let settings_focused = self.focused == 0;
                ui::draw_row_panel(canvas, settings_rect, settings_focused);
                ui::draw_gear_icon(
                    canvas,
                    settings_rect,
                    if settings_focused { ui::WHITE } else { ui::DIM },
                    if settings_focused { ui::PANEL_BG_FOCUSED } else { ui::PANEL_BG },
                );
                if self.entries.is_empty() {
                    ui::draw_text(
                        canvas,
                        texture_creator,
                        font_label,
                        "Searching for hosts on your network…",
                        left,
                        ui::ROWS_TOP_Y - 40,
                        ui::DIM,
                    )?;
                }
                let rows = self.host_rows();
                // `focused` is a nav index (0 = Settings button); the row list below
                // only highlights something when focus is actually on it.
                let list_focus = self.focused.checked_sub(1).unwrap_or(usize::MAX);
                ui::draw_rows(
                    canvas,
                    texture_creator,
                    font_label,
                    font_value,
                    &rows,
                    list_focus,
                    left,
                    ui::ROWS_TOP_Y,
                    width,
                    ui::ROW_H,
                    ui::ROW_GAP,
                )?;
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_value,
                    "[Confirm] Select   [Secondary] Forget host   [Back] Settings",
                    left,
                    (screen_h as i32) - 60,
                    ui::DIM,
                )?;
            }
            Screen::Pairing => {
                ui::draw_back_button(canvas, texture_creator, font_value, self.hover_back)?;
                ui::draw_text(canvas, texture_creator, font_title, "Pair with host", left, 48, ui::WHITE)?;
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_label,
                    "Enter the PIN shown in the host's pairing dialog.",
                    left,
                    100,
                    ui::DIM,
                )?;
                let digit_w = 70i32;
                let digit_gap = 16i32;
                let total_w = 4 * digit_w + 3 * digit_gap;
                let start_x = left + (width as i32 - total_w) / 2;
                for (i, digit) in self.pin_digits.iter().enumerate() {
                    let x = start_x + i as i32 * (digit_w + digit_gap);
                    let rect = Rect::new(x, 160, digit_w as u32, 90);
                    let focused = i == self.pin_digit_index;
                    ui::draw_row_panel(canvas, rect, focused);
                    let text = digit.to_string();
                    let tw = font_title.size_of(&text).map(|(w, _)| w).unwrap_or(0);
                    ui::draw_text(
                        canvas,
                        texture_creator,
                        font_title,
                        &text,
                        x + (digit_w - tw as i32) / 2,
                        160 + (90 - font_title.height()) / 2,
                        ui::WHITE,
                    )?;
                }
                if let Some(status) = &self.pairing_status {
                    let color = if self.pairing_busy { ui::DIM } else { ui::ERROR_RED };
                    ui::draw_text(canvas, texture_creator, font_label, status, left, 280, color)?;
                }
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_value,
                    "[Up/Down] Digit   [Left/Right] Change   [Confirm] Pair   [Back] Cancel",
                    left,
                    (screen_h as i32) - 60,
                    ui::DIM,
                )?;
            }
            Screen::Settings => {
                ui::draw_back_button(canvas, texture_creator, font_value, self.settings_focused == 0)?;
                ui::draw_text(canvas, texture_creator, font_title, "Settings", left, 48, ui::WHITE)?;
                // Confirmed platform limitation, not a client bug: neither SDL-webOS
                // nor any webOS system service exposes a way for a native app to
                // change the TV panel's actual output refresh rate (only read it) —
                // so Frame rate here only affects the stream's encode pacing/wire
                // negotiation, not the panel's real scan-out rate.
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_value,
                    "Frame rate paces the stream only — the TV panel's own refresh rate isn't app-controllable.",
                    left,
                    100,
                    ui::DIM,
                )?;
                let rows = ui::settings_rows(&self.settings);
                // `settings_focused` is a nav index (0 = top-left Back button); the
                // row list only highlights something when focus is actually on it.
                let list_focus = self.settings_focused.checked_sub(1).unwrap_or(usize::MAX);
                ui::draw_rows(
                    canvas,
                    texture_creator,
                    font_label,
                    font_value,
                    &rows,
                    list_focus,
                    left,
                    ui::ROWS_TOP_Y,
                    width,
                    ui::ROW_H,
                    ui::ROW_GAP,
                )?;
                if let Some(dd) = &self.dropdown {
                    let options = ui::dropdown_options(dd.row);
                    let overlay_y = ui::ROWS_TOP_Y + (dd.row as i32 + 1) * (ui::ROW_H as i32 + ui::ROW_GAP);
                    ui::draw_dropdown_overlay(
                        canvas,
                        texture_creator,
                        font_value,
                        &options,
                        dd.focused,
                        left,
                        overlay_y,
                        width,
                    )?;
                }
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_value,
                    if self.dropdown.is_some() {
                        "[Up/Down] Choose   [Confirm] Select   [Back] Cancel"
                    } else {
                        "[Left/Right] Change   [Confirm] Open/Toggle   [Back] Done"
                    },
                    left,
                    (screen_h as i32) - 60,
                    ui::DIM,
                )?;
            }
            Screen::AddHost => {
                ui::draw_back_button(canvas, texture_creator, font_value, self.hover_back)?;
                ui::draw_text(canvas, texture_creator, font_title, "Add host", left, 48, ui::WHITE)?;
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_label,
                    "Enter the host's IP address and port.",
                    left,
                    100,
                    ui::DIM,
                )?;
                let text = self.add_host.display_text();
                let focus_char = self.add_host.focus_char_index();
                let rect = Rect::new(left, 160, width, 90);
                ui::draw_row_panel(canvas, rect, true);
                let text_w = font_title.size_of(&text).map(|(w, _)| w).unwrap_or(0);
                ui::draw_highlighted_text(
                    canvas,
                    texture_creator,
                    font_title,
                    &text,
                    focus_char,
                    left + (width as i32 - text_w as i32) / 2,
                    160 + (90 - font_title.height()) / 2,
                    ui::WHITE,
                    ui::BRAND,
                )?;
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_value,
                    "[Up/Down] Field   [Left/Right] Change   [Confirm] Add host   [Back] Cancel",
                    left,
                    (screen_h as i32) - 60,
                    ui::DIM,
                )?;
            }
            Screen::Library => {
                ui::draw_back_button(canvas, texture_creator, font_value, self.library_focused == 0)?;
                ui::draw_text(canvas, texture_creator, font_title, "Select", left, 48, ui::WHITE)?;
                if let Some(status) = &self.library_status {
                    ui::draw_text(canvas, texture_creator, font_label, status, left, 100, ui::DIM)?;
                }
                let mut rows = vec![Row::action("Desktop", "")];
                rows.extend(self.games.iter().map(|g| Row::action(g.title.clone(), "")));
                // `library_focused` is a nav index (0 = top-left Back button, 1 =
                // Desktop, 2.. = games); the row list only highlights something
                // when focus is actually on it.
                let list_focus = self.library_focused.checked_sub(1).unwrap_or(usize::MAX);
                ui::draw_rows(
                    canvas,
                    texture_creator,
                    font_label,
                    font_value,
                    &rows,
                    list_focus,
                    left,
                    ui::ROWS_TOP_Y,
                    width,
                    ui::ROW_H,
                    ui::ROW_GAP,
                )?;
                ui::draw_text(
                    canvas,
                    texture_creator,
                    font_value,
                    "[Confirm] Launch   [Back] Cancel",
                    left,
                    (screen_h as i32) - 60,
                    ui::DIM,
                )?;
            }
        }
        canvas.present();
        Ok(())
    }
}
