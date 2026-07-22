//! The pre-stream UI flow: a persistent Home screen (sidebar of known hosts +
//! detail grid of the selected host's games) with Pairing/Settings/Add-host as
//! centered modals on top of it — modeled on moonlight-tv's actual layout (see
//! `ui.rs`'s module docs). `ui.rs` owns drawing/input-mapping primitives,
//! `store.rs` owns persistence, `discovery.rs` owns mDNS.
use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::Result;
use sdl2::rect::Rect;
use tiny_skia::Pixmap;

use crate::library::GameEntry;
use crate::store::{self, KnownHost, Settings};
use crate::ui::{self, AddHostState, HostEntry, MenuEvent, Painter};

pub enum Screen {
    Home,
    Pairing,
    Settings,
    AddHost,
    /// "This configured host is unreachable — send it a Wake-on-LAN signal?" — see
    /// `WakeState`'s docs.
    Wake,
    /// "Forget this host?" — a centered Forget/Cancel confirmation, entered by
    /// a long-press of OK on a sidebar host row (see `main.rs`'s hold-timer).
    /// Which host is `App::host_menu_index`.
    ForgetHost,
}

/// How often the magic packet is re-sent while a wake is in flight — see
/// `App::tick_wake`.
const WAKE_RESEND_INTERVAL: Duration = Duration::from_secs(15);

/// How long a *silent* auto-send (`Settings::wol_auto_send`) waits before giving up on
/// staying quiet and surfacing the wake prompt anyway — see `App::tick_wake`.
const WAKE_ESCALATE_AFTER: Duration = Duration::from_secs(60);

/// How often `App::tick_wake` actively re-checks reachability, independent of the WOL
/// timers above and of whether a MAC is even on record.
const WAKE_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// State for the "configured host is unreachable — wake it?" flow: both the interactive
/// prompt (`Screen::Wake`) and the silent background wait behind an auto-send live here,
/// distinguished by `silent`. Entered from `App::start_wake` whenever a known/paired
/// host's library fetch fails as genuinely unreachable, replacing the old plain grid
/// error message in every case — even with no MAC on record, where `render_wake` just
/// hides the send/auto-send controls instead.
pub struct WakeState {
    host: String,
    port: u16,
    name: String,
    mac: Vec<String>,
    /// The original library error, restored into `home_status` if the user backs out
    /// without sending — so declining the prompt looks exactly like it did before this
    /// flow existed.
    reason: String,
    /// Row focus on the modal: `0` = "Send Wake-on-LAN now", `1` = the "Always send
    /// automatically" toggle.
    focused: usize,
    /// Whether a packet has gone out for the current wait window.
    sent: bool,
    /// When the current wait window started (its first send) — drives the 60s
    /// escalation.
    since: Option<Instant>,
    last_attempt: Option<Instant>,
    /// `true` while this wait is running quietly because `wol_auto_send` fired it with
    /// no prompt shown — `App::tick_wake` flips it (and shows the prompt) once
    /// `WAKE_ESCALATE_AFTER` passes with the host still unreachable, so the user gets a
    /// chance to turn auto-send back off instead of it failing forever in silence.
    silent: bool,
    /// When the last active reachability probe went out — see `App::tick_wake`.
    last_probe: Option<Instant>,
    /// An in-flight reachability probe, if one is currently out.
    probe_rx: Option<std::sync::mpsc::Receiver<crate::library::GamesLoaded>>,
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

/// A grid-card confirm that's still waiting on its pre-flight reachability check
/// before actually handing its `ConnectTarget` off to start a stream — see
/// `App::confirm_grid_card`/`App::drain_launch_check`.
struct PendingLaunch {
    host: String,
    port: u16,
    fingerprint: [u8; 32],
    launch: Option<String>,
    rx: std::sync::mpsc::Receiver<crate::library::GamesLoaded>,
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
    /// `None` if `discovery::browse` couldn't even start (`ServiceDaemon::new` failed —
    /// `discovered` is then a permanently-empty channel, harmless for `drain_discovery`'s
    /// `try_recv` loop). `Some` lets `Drop` shut the background mDNS thread down explicitly
    /// instead of relying on `discovered` being dropped — dropping the receiver alone doesn't
    /// reliably stop it (see `discovery::browse`'s docs), and it was confirmed still burning
    /// CPU/network well into active game-streaming sessions, long after this `App` was gone.
    discovery_daemon: Option<mdns_sd::ServiceDaemon>,
    pub entries: Vec<HostEntry>,
    pub home_focus: HomeFocus,
    /// The sidebar host whose games are shown in the grid — `None` until one is
    /// selected (or restored from `store::load_selected_host` at startup).
    pub selected_host: Option<(String, u16)>,
    pub games: Vec<GameEntry>,
    /// In-flight `library::fetch_games` call, if any — see `select_host`/`drain_games`.
    games_rx: Option<std::sync::mpsc::Receiver<crate::library::GamesLoaded>>,
    /// Library loading/error message shown in the grid area in place of cards.
    pub home_status: Option<String>,
    /// Decoded cover art, keyed by `GameEntry::id` — a `tiny_skia::Pixmap` composited
    /// straight into the frame `Painter`; see `art.rs` docs on why no separate
    /// GPU-texture-building step is needed here.
    pub art: std::collections::HashMap<String, Pixmap>,
    art_rx: Option<std::sync::mpsc::Receiver<crate::art::ArtLoaded>>,
    pending_launch: Option<PendingLaunch>,
    /// Set by `drain_launch_check` on success — `main.rs` picks it up via
    /// `take_ready_launch` and starts the stream. Separate from `pending_launch`
    /// since that's cleared as soon as a result arrives, before `main.rs` can act.
    launch_ready: Option<ConnectTarget>,
    pub settings: Settings,
    pub settings_focused: usize,
    pub dropdown: Option<DropdownState>,
    /// The sidebar row `Screen::ForgetHost` is confirming forgetting — set
    /// alongside `screen = Screen::ForgetHost` (see `App::open_forget_host`),
    /// `None` otherwise.
    pub host_menu_index: Option<usize>,
    /// Which `Screen::ForgetHost` button has focus: `0` = "Forget", `1` =
    /// "Cancel". Defaults to Cancel (see `open_forget_host`) — a destructive
    /// action shouldn't be one more accidental OK press away.
    pub host_menu_focused: usize,
    pub add_host: AddHostState,
    /// The active "host unreachable — wake it?" prompt/wait, if any — see `WakeState`.
    pub wake: Option<WakeState>,
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
    /// Cached last-rendered Home (sidebar+grid) background, reused while a modal
    /// (Settings/Pairing/AddHost/Wake) is on top of it instead of redrawing Home
    /// from scratch every frame — see `render`'s docs. Lazily sized on first use
    /// since `App::new` doesn't know the screen dimensions yet.
    home_layer: Option<Painter>,
    /// Set whenever anything Home would draw differently might have changed
    /// (`entries`/`games`/`art`/`selected_host`/`home_status`/`home_focus`) —
    /// `render` redraws `home_layer` from `render_home` when this is true, then
    /// clears it. Always `true` on a Home-screen frame (that frame draws Home
    /// directly, not through the cache, so the cache must be treated as stale
    /// until the next time it's actually needed) and set explicitly at every
    /// other mutation site, since those can run while a modal is active (e.g. a
    /// library fetch finishing while the user is in Settings).
    home_dirty: bool,
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(daemon) = &self.discovery_daemon {
            let _ = daemon.shutdown();
        }
    }
}

impl App {
    pub fn new(identity: (String, String), log: &mut std::fs::File) -> Self {
        let known_hosts = store::load_known_hosts();
        let entries = known_hosts.iter().cloned().map(HostEntry::Known).collect();
        // A second handle onto the same log file (both just append — see
        // `main.rs::log_path`) for the mdns background thread to log through, since
        // it can't share this `&mut File` across threads.
        let discovery_log = log.try_clone().expect("clone log file handle for mdns thread");
        let (discovered, discovery_daemon) = match crate::discovery::browse(discovery_log) {
            Some((rx, daemon)) => (rx, Some(daemon)),
            None => (std::sync::mpsc::channel().1, None),
        };
        let mut app = Self {
            screen: Screen::Home,
            known_hosts,
            discovered,
            discovery_daemon,
            entries,
            home_focus: HomeFocus::Sidebar(0),
            selected_host: None,
            games: Vec::new(),
            games_rx: None,
            home_status: None,
            art: std::collections::HashMap::new(),
            art_rx: None,
            pending_launch: None,
            launch_ready: None,
            settings: store::load_settings(),
            settings_focused: 0,
            dropdown: None,
            host_menu_index: None,
            host_menu_focused: 1,
            add_host: AddHostState::default(),
            wake: None,
            pin_digits: [0; 4],
            pin_digit_index: 0,
            pairing_status: None,
            pairing_busy: false,
            pairing_entry: 0,
            hover_close: false,
            identity,
            home_layer: None,
            home_dirty: true,
        };
        // Restore the last-active sidebar host (if it's still known and paired)
        // so relaunching the app lands back on its game grid.
        if let Some((host, port)) = store::load_selected_host() {
            if let Some(h) = app
                .known_hosts
                .iter()
                .find(|h| h.host == host && h.port == port && h.fingerprint.is_some())
            {
                let (host, port, mgmt_port) = (h.host.clone(), h.port, h.mgmt_port);
                app.select_host(host, port, mgmt_port, log);
            }
        }
        app
    }

    /// Merges freshly-discovered hosts into the entry list (known hosts keep their
    /// paired status; a discovered host not yet known gets appended), learns each
    /// known host's Wake-on-LAN MAC(s) from its live advert while it's awake to
    /// advertise them, and — if a wake is in flight (`self.wake`) — notices when the
    /// waking host reappears on mDNS and reconnects. Returns whether the sidebar
    /// actually changed — `main.rs`'s render loop uses this to skip a redraw when a
    /// discovery tick found nothing new (see its dirty-flag docs).
    pub fn drain_discovery(&mut self, log: &mut std::fs::File) -> bool {
        let mut changed = false;
        let mut mac_learned = false;
        let mut woke = None;
        // `found.addr` throughout this loop is deliberate, not a typo for a nonexistent
        // `found.host` — `DiscoveredHost` (discovery.rs) only has `addr`, `WakeState`/
        // `KnownHost` only have `host`; both hold the same kind of value (network address).
        while let Ok(found) = self.discovered.try_recv() {
            #[allow(clippy::suspicious_operation_groupings)]
            if let Some(w) = &self.wake {
                if found.addr == w.host && found.port == w.port {
                    woke = Some((found.addr.clone(), found.port, found.mgmt_port));
                }
            }
            #[allow(clippy::suspicious_operation_groupings)]
            let known = self
                .known_hosts
                .iter_mut()
                .find(|h| h.host == found.addr && h.port == found.port);
            if let Some(known) = known {
                if !found.mac.is_empty() && known.mac != found.mac {
                    known.mac.clone_from(&found.mac);
                    mac_learned = true;
                }
            }
            #[allow(clippy::suspicious_operation_groupings)]
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
                changed = true;
            }
        }
        if mac_learned {
            let _ = store::save_known_hosts(&self.known_hosts);
        }
        if let Some((host, port, mgmt_port)) = woke {
            self.wake_succeeded(host, port, mgmt_port, "mDNS", log);
            changed = true;
        }
        if changed {
            self.home_dirty = true;
        }
        changed
    }

    /// Ends an in-flight wake because the host is actually back — whether that was
    /// noticed passively (`drain_discovery` seeing a fresh mDNS resolve) or actively
    /// (`tick_wake`'s reachability probe succeeding). `source` is just for the log line.
    fn wake_succeeded(&mut self, host: String, port: u16, mgmt_port: Option<u16>, source: &str, log: &mut std::fs::File) {
        let _ = writeln!(log, "wake succeeded: {host}:{port} back ({source})");
        self.wake = None;
        self.screen = Screen::Home;
        self.select_host(host, port, mgmt_port, log);
    }

    /// Drains any cover art that's finished decoding since the last tick — called
    /// alongside `drain_discovery`. Returns whether any new art actually arrived
    /// (see `drain_discovery`'s docs on why).
    pub fn drain_art(&mut self) -> bool {
        let Some(rx) = &self.art_rx else { return false };
        let mut changed = false;
        while let Ok(loaded) = rx.try_recv() {
            self.art.insert(loaded.game_id, loaded.pixmap);
            changed = true;
        }
        if changed {
            self.home_dirty = true;
        }
        changed
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
            Some((h, p)) => self
                .entries
                .iter()
                .position(|e| e.host() == h && e.port() == *p)
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Whether the sidebar currently has focus on an actual host row (not
    /// "+ Add host"/"Settings") — the only situation where `main.rs` holding
    /// OK down means "open the Forget confirmation" rather than that button's
    /// normal short-press action. Returns the focused row's index into
    /// `entries`.
    pub fn host_row_focused(&self) -> Option<usize> {
        if !matches!(self.screen, Screen::Home) {
            return None;
        }
        match self.home_focus {
            HomeFocus::Sidebar(i) if i < self.entries.len() => Some(i),
            _ => None,
        }
    }

    /// If `(x, y)` lands on a sidebar host row, focuses it and returns its index;
    /// `None` otherwise, leaving `home_focus` untouched. `main.rs`'s
    /// `MouseButtonDown` handler uses this to decide whether to start the
    /// hold-timer for the Forget-host gesture.
    pub fn focus_host_row_at(&mut self, x: i32, y: i32, screen_h: u32) -> Option<usize> {
        if !matches!(self.screen, Screen::Home) {
            return None;
        }
        let idx = ui::hit_test_sidebar_row(x, y, self.sidebar_len(), screen_h)?;
        if idx >= self.entries.len() {
            return None;
        }
        self.home_focus = HomeFocus::Sidebar(idx);
        Some(idx)
    }

    /// Enters `Screen::ForgetHost` for the sidebar row at `idx` — called from
    /// `main.rs` once an OK hold on that row crosses `LONG_PRESS_CONFIRM`.
    pub fn open_forget_host(&mut self, idx: usize) {
        self.host_menu_index = Some(idx);
        self.host_menu_focused = 1;
        self.screen = Screen::ForgetHost;
    }

    /// Handles one menu event on the `Screen::ForgetHost` confirmation.
    /// Left/Right toggle which button has focus; Confirm acts on it (forgets
    /// the host, or just backs out for Cancel); Back is the same as Cancel.
    pub fn handle_forget_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left | MenuEvent::Right => self.host_menu_focused = 1 - self.host_menu_focused,
            MenuEvent::Confirm => {
                if self.host_menu_focused == 0 {
                    if let Some(idx) = self.host_menu_index {
                        self.forget_host(idx);
                    }
                }
                self.host_menu_index = None;
                self.screen = Screen::Home;
            }
            MenuEvent::Back => {
                self.host_menu_index = None;
                self.screen = Screen::Home;
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Handles one menu event on the Home screen (sidebar + grid). Returns a
    /// `ConnectTarget` when a grid card is confirmed.
    pub fn handle_home_event(
        &mut self,
        ev: MenuEvent,
        screen_w: u32,
        log: &mut std::fs::File,
    ) -> Option<ConnectTarget> {
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
                HomeFocus::Grid(i) => self.confirm_grid_card(i, log),
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

    /// Applies a `Back` to whichever screen is current — the single shared
    /// definition of "what Back means here" for every caller that needs it
    /// pre-emptively rather than through the normal per-screen `MenuEvent`
    /// dispatch: `main.rs`'s keyboard/gamepad Back shortcut on Home (straight
    /// to Settings) and a modal's close (X) button click
    /// (`handle_mouse_click`'s `hover_close` branch below).
    pub fn back(&mut self, log: &mut std::fs::File) -> Option<ConnectTarget> {
        match self.screen {
            // Home has nothing to "back out" of (it's the root screen) — Back is a
            // shortcut straight to Settings instead, since it's otherwise reachable
            // only via the sidebar's trailing row.
            Screen::Home => {
                self.screen = Screen::Settings;
                self.dropdown = None;
                self.settings_focused = 0;
                None
            }
            Screen::Pairing => {
                self.handle_pairing_event(MenuEvent::Back, log);
                None
            }
            Screen::Settings => {
                self.handle_settings_event(MenuEvent::Back);
                None
            }
            Screen::AddHost => {
                self.handle_add_host_event(MenuEvent::Back);
                None
            }
            Screen::Wake => {
                self.handle_wake_event(MenuEvent::Back, log);
                None
            }
            Screen::ForgetHost => {
                self.handle_forget_host_event(MenuEvent::Back);
                None
            }
        }
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
    fn select_host(&mut self, host: String, port: u16, mgmt_port: Option<u16>, log: &mut std::fs::File) {
        let _ = store::save_selected_host(&host, port);
        self.selected_host = Some((host.clone(), port));
        self.home_status = Some("Loading library…".into());
        self.games = Vec::new();
        self.art.clear();
        self.art_rx = None;
        self.home_focus = HomeFocus::Grid(0);
        self.home_dirty = true;

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let fingerprint = self
            .known_hosts
            .iter()
            .find(|h| h.host == host && h.port == port)
            .and_then(|h| h.fingerprint);
        let mgmt_port = mgmt_port.unwrap_or(crate::library::DEFAULT_MGMT_PORT);
        let _ = writeln!(log, "library: fetching from {host}:{mgmt_port}…");
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
    pub fn drain_games(&mut self, log: &mut std::fs::File) -> bool {
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
            Ok(games) => {
                let _ = writeln!(log, "library: {} games from {host}:{mgmt_port}", games.len());
                let identity = (self.identity.0.clone(), self.identity.1.clone());
                let fingerprint = self
                    .known_hosts
                    .iter()
                    .find(|h| h.host == host && h.port == port)
                    .and_then(|h| h.fingerprint);
                self.art_rx = Some(crate::art::load_art_async(
                    host,
                    mgmt_port,
                    identity,
                    fingerprint,
                    games.clone(),
                ));
                self.games = games;
                self.home_status = None;
            }
            Err(e) => {
                let _ = writeln!(log, "library fetch failed ({host}:{mgmt_port}): {e}");
                self.handle_library_error(host, port, e, log);
            }
        }
        self.home_dirty = true;
        true
    }

    /// Shared handling for a failed library fetch/reachability check, used by both
    /// `drain_games` and `drain_launch_check`. `Unreachable` opens the Wake dialog
    /// (even with no MAC on record — `start_wake`/`render_wake` just hide the send
    /// controls then); `NotPaired`/`PinMismatch`/`Http` mean the host answered, so
    /// Wake-on-LAN wouldn't help — those stay a plain status line.
    fn handle_library_error(&mut self, host: String, port: u16, e: crate::library::LibraryError, log: &mut std::fs::File) {
        let reason = format!("{e} (Desktop is still available.)");
        if matches!(e, crate::library::LibraryError::Unreachable(_)) {
            let mac = self
                .known_hosts
                .iter()
                .find(|h| h.host == host && h.port == port)
                .map(|h| h.mac.clone())
                .unwrap_or_default();
            self.start_wake(host, port, mac, reason, log);
        } else {
            self.home_status = Some(reason);
        }
    }

    /// Enters the "host unreachable — wake it?" flow (see `WakeState`'s docs). With
    /// `Settings::wol_auto_send` off, this shows the prompt right away, replacing what
    /// used to be a plain error message. With it on, the packet fires immediately and
    /// silently — the prompt only appears if the host still hasn't come back a minute
    /// later (`tick_wake`), which is also the one place that setting can be turned back
    /// off (no separate settings row for it — see `Settings::wol_auto_send`).
    fn start_wake(&mut self, host: String, port: u16, mac: Vec<String>, reason: String, log: &mut std::fs::File) {
        let name = self
            .known_hosts
            .iter()
            .find(|h| h.host == host && h.port == port)
            .map_or_else(|| host.clone(), |h| h.name.clone());
        // Nothing to send without a MAC on record — never pretend to auto-send in
        // that case, just show the (mac-less) interactive explanation instead.
        let auto = self.settings.wol_auto_send && !mac.is_empty();
        let mut wake = WakeState {
            host,
            port,
            name,
            mac,
            reason,
            focused: if auto { 1 } else { 0 },
            sent: false,
            since: None,
            last_attempt: None,
            silent: auto,
            // Baseline for `WAKE_PROBE_INTERVAL` — the first active probe fires
            // `WAKE_PROBE_INTERVAL` from now, not immediately.
            last_probe: Some(Instant::now()),
            probe_rx: None,
        };
        if auto {
            Self::send_wake(&mut wake, log);
        } else {
            self.screen = Screen::Wake;
        }
        self.wake = Some(wake);
    }

    /// Fires (or re-fires) the magic packet for an in-flight wake, bumping its resend
    /// timer — shared by the modal's explicit "Send" action and `tick_wake`'s periodic
    /// resend.
    fn send_wake(wake: &mut WakeState, log: &mut std::fs::File) {
        crate::wol::wake_and_log(&wake.mac, wake.host.parse().ok(), &wake.name, log);
        let now = Instant::now();
        wake.sent = true;
        wake.since.get_or_insert(now);
        wake.last_attempt = Some(now);
    }

    /// Advances an in-flight wake: resends the WOL packet every `WAKE_RESEND_INTERVAL`
    /// (once a MAC is on record — see `WakeState::mac`'s docs), escalates a silent
    /// auto-send to the visible prompt after `WAKE_ESCALATE_AFTER`, and — regardless of
    /// either — actively re-checks reachability every `WAKE_PROBE_INTERVAL` via
    /// `wake_probe`, ending the wake via `wake_succeeded` on success. This runs whether
    /// or not `Screen::Wake` is actually showing (same as the WOL timers), since a
    /// silent auto-send wait has no modal open at all; `drain_discovery`'s passive mDNS
    /// check can also end a wake independently, whichever notices first. Called every UI
    /// tick; returns whether anything visibly changed (same contract as
    /// `drain_discovery`/`drain_art`).
    pub fn tick_wake(&mut self, log: &mut std::fs::File) -> bool {
        let Some(wake) = &mut self.wake else { return false };
        let now = Instant::now();
        let mut changed = false;

        if let Some(rx) = &wake.probe_rx {
            if let Ok(loaded) = rx.try_recv() {
                wake.probe_rx = None;
                changed = true;
                if loaded.result.is_ok() {
                    let (host, port) = (wake.host.clone(), wake.port);
                    let mgmt_port = self
                        .known_hosts
                        .iter()
                        .find(|h| h.host == host && h.port == port)
                        .and_then(|h| h.mgmt_port);
                    self.wake_succeeded(host, port, mgmt_port, "reachability probe", log);
                    return true;
                }
                wake.last_probe = Some(now);
            }
        }
        let Some(wake) = &mut self.wake else { return changed };

        // Only ever *resend*, gated on `wake.sent` — without it, this fired the first
        // WOL packet on the very next tick after `start_wake` regardless of
        // `Settings::wol_auto_send` (`last_attempt: None` reads as "due"). The first
        // send is either `start_wake`'s own immediate call (auto-send on) or the user's
        // explicit Confirm on "Send" (`handle_wake_event`).
        if !wake.mac.is_empty() {
            let due = wake.sent && wake.last_attempt.is_some_and(|t| now.duration_since(t) >= WAKE_RESEND_INTERVAL);
            if due {
                Self::send_wake(wake, log);
                changed = true;
            }
        }

        if wake.silent && wake.since.is_some_and(|t| now.duration_since(t) >= WAKE_ESCALATE_AFTER) {
            wake.silent = false;
            wake.focused = 1; // land on the toggle — the likely reason the user is here
            self.screen = Screen::Wake;
            changed = true;
        }

        if wake.probe_rx.is_none() && wake.last_probe.is_some_and(|t| now.duration_since(t) >= WAKE_PROBE_INTERVAL) {
            let (host, port) = (wake.host.clone(), wake.port);
            wake.probe_rx = Some(Self::wake_probe(&self.known_hosts, &self.identity, &host, port));
            wake.last_probe = Some(now);
        }
        changed
    }

    /// Kicks off one reachability probe for `(host, port)` — the same mTLS library
    /// fetch `confirm_grid_card`'s pre-flight check uses, reused here as `tick_wake`'s
    /// active "is it back yet" signal. A plain associated function (not `&self`) so it
    /// can be called while `tick_wake` already holds `&mut self.wake`.
    fn wake_probe(
        known_hosts: &[KnownHost],
        identity: &(String, String),
        host: &str,
        port: u16,
    ) -> std::sync::mpsc::Receiver<crate::library::GamesLoaded> {
        let known = known_hosts.iter().find(|h| h.host == host && h.port == port);
        let mgmt_port = known.and_then(|h| h.mgmt_port).unwrap_or(crate::library::DEFAULT_MGMT_PORT);
        let fingerprint = known.and_then(|h| h.fingerprint);
        crate::library::load_games_async(host.to_string(), port, mgmt_port, identity.clone(), fingerprint)
    }

    /// Handles one menu event on the Wake modal: Up/Down move focus between the two
    /// rows, Confirm sends (row 0) or flips the auto-send toggle (row 1), Left/Right
    /// also flip the toggle when it's focused (matching the Settings modal's toggle
    /// idiom), Back dismisses back to the plain error text `WakeState::reason` carries.
    pub fn handle_wake_event(&mut self, ev: MenuEvent, log: &mut std::fs::File) {
        let Some(wake) = self.wake.as_mut() else { return };
        // No MAC on record for this host yet — there's nothing to send or automate
        // (see `render_wake`, which hides those rows in this case too), so every
        // event but Back (handled below, same as always) is a no-op.
        if wake.mac.is_empty() && ev != MenuEvent::Back {
            return;
        }
        match ev {
            MenuEvent::Up | MenuEvent::Down => wake.focused = 1 - wake.focused,
            MenuEvent::Confirm | MenuEvent::Left | MenuEvent::Right if wake.focused == 1 => {
                self.settings.wol_auto_send = !self.settings.wol_auto_send;
                let _ = store::save_settings(&self.settings);
            }
            MenuEvent::Confirm => Self::send_wake(wake, log),
            MenuEvent::Back => {
                self.home_status = self.wake.take().map(|w| w.reason);
                self.screen = Screen::Home;
            }
            MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => {}
        }
    }

    /// Confirms a grid card ("Desktop" at `idx == 0`, or a game). Kicks off a fresh
    /// reachability check first rather than handing back a `ConnectTarget` directly —
    /// the grid being populated only proves the host answered once, when its library
    /// was last fetched, and it could have gone offline since (`session::connect`'s
    /// failure currently propagates uncaught, taking the whole process down — see
    /// `main.rs`'s docs). `main.rs`'s tick loop drains the result via
    /// `drain_launch_check`/`take_ready_launch`. No-ops if a check is already in flight.
    fn confirm_grid_card(&mut self, idx: usize, log: &mut std::fs::File) {
        if self.pending_launch.is_some() {
            return;
        }
        let Some((host, port)) = self.selected_host.clone() else { return };
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
        let _ = writeln!(log, "launch: checking {host}:{port} is still reachable before connecting…");
        self.home_status = Some("Checking connection…".into());
        self.home_dirty = true;
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
    pub fn drain_launch_check(&mut self, log: &mut std::fs::File) -> bool {
        let Some(pending) = &self.pending_launch else { return false };
        let Ok(loaded) = pending.rx.try_recv() else { return false };
        let PendingLaunch {
            host,
            port,
            fingerprint,
            launch,
            ..
        } = self.pending_launch.take().expect("just matched Some above");
        match loaded.result {
            Ok(_) => {
                if self.selected_host.as_ref().is_some_and(|(h, p)| *h == host && *p == port) {
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
                let _ = writeln!(log, "launch check failed ({host}:{port}): {e}");
                self.handle_library_error(host, port, e, log);
            }
        }
        self.home_dirty = true;
        true
    }

    /// Takes the `ConnectTarget` a finished `drain_launch_check` produced, if any —
    /// `main.rs`'s tick loop calls this right after `drain_launch_check` and breaks its
    /// event loop with it to actually start the stream.
    pub fn take_ready_launch(&mut self) -> Option<ConnectTarget> {
        self.launch_ready.take()
    }

    fn forget_host(&mut self, idx: usize) {
        let HostEntry::Known(h) = &self.entries[idx] else {
            return;
        };
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
        self.home_dirty = true;
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
        let mac = entry.mac().to_vec();
        let pin: String = self.pin_digits.iter().map(std::string::ToString::to_string).collect();
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
                        mac,
                    },
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
                self.settings_focused = if self.settings_focused == 0 {
                    total - 1
                } else {
                    self.settings_focused - 1
                };
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
                ui::ROW_RESOLUTION | ui::ROW_FRAMERATE | ui::ROW_VIDEO_BACKEND => {
                    let focused = ui::dropdown_current_index(&self.settings, self.settings_focused);
                    self.dropdown = Some(DropdownState {
                        row: self.settings_focused,
                        focused,
                    });
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

    /// Handles one menu event on the manual add-host modal — a plain, growing
    /// IP digit string with no port field (see `ui::AddHostState`'s docs).
    /// Left/Right stand in for backspace/"next octet" (no dot key on the
    /// remote); Confirm submits once four octets have been typed.
    pub fn handle_add_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.add_host.backspace(),
            MenuEvent::Right => self.add_host.advance_octet(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
            MenuEvent::Confirm => self.confirm_add_host(),
            MenuEvent::Back => self.screen = Screen::Home,
        }
    }

    /// Direct digit entry (the Magic Remote's number buttons) on the add-host
    /// modal — same auto-advance idiom as `enter_pin_digit`.
    pub fn enter_add_host_digit(&mut self, digit: u8) {
        self.add_host.enter_digit(digit);
    }

    /// No-op until all four octets have been typed (`ui::AddHostState::is_complete`)
    /// — Confirm on a still-partial address just does nothing rather than
    /// connecting to a truncated/zero-padded guess.
    fn confirm_add_host(&mut self) {
        if !self.add_host.is_complete() {
            return;
        }
        let (host, port) = self.add_host.host_and_port();
        store::upsert_known_host(
            &mut self.known_hosts,
            KnownHost {
                name: host.clone(),
                host: host.clone(),
                port,
                fingerprint: None,
                mgmt_port: None,
                mac: Vec::new(),
            },
        );
        let _ = store::save_known_hosts(&self.known_hosts);
        self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
        self.home_focus = HomeFocus::Sidebar(
            self.entries
                .iter()
                .position(|e| e.host() == host && e.port() == port)
                .unwrap_or(0),
        );
        self.screen = Screen::Home;
    }

    // ---------------------------------------------------------------- mouse --

    /// The pairing modal's card rect — shared by `render_pairing` and mouse
    /// hit-testing. Sized generously for `ui::draw_modal_header`'s layout (see its
    /// docs) plus the PIN boxes and an up-to-two-line pairing-failure status below.
    fn pairing_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::modal_card_rect(screen_w, screen_h, 0.36, 480)
    }

    /// The add-host modal's card rect — shared by `render_add_host` and mouse
    /// hit-testing.
    fn add_host_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::modal_card_rect(screen_w, screen_h, 0.46, 260)
    }

    /// The wake modal's card rect — shared by `render_wake` and mouse hit-testing.
    /// Sized generously for `ui::draw_modal_header`'s layout (its title uses the
    /// large `font_title`) plus the two action rows below.
    fn wake_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::modal_card_rect(screen_w, screen_h, 0.42, 520)
    }

    /// The "Forget this host?" confirmation's card rect — shared by
    /// `render_forget_host` and mouse hit-testing. Sized generously for
    /// `ui::draw_modal_header`'s layout plus an up-to-two-line host-name subtitle.
    fn forget_host_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::modal_card_rect(screen_w, screen_h, 0.34, 340)
    }

    /// The settings modal's card/content rects — shared by `render` and mouse
    /// hit-testing so they can never disagree.
    fn settings_layout(screen_w: u32, screen_h: u32) -> (Rect, Rect) {
        let content_h = ui::SETTINGS_ROW_COUNT as u32 * (ui::SETTINGS_ROW_H + ui::SETTINGS_ROW_GAP as u32);
        // Room for the title/divider above and the high-bitrate caution below.
        let card_h = content_h + 200;
        let card = ui::modal_card_rect(screen_w, screen_h, 0.56, card_h);
        let content = Rect::new(
            card.x() + 40,
            card.y() + 120,
            card.width().saturating_sub(80),
            content_h,
        );
        (card, content)
    }

    /// Updates focus/hover to whatever the Magic Remote's pointer is over.
    /// Returns whether that actually changed anything visible — Magic Remote
    /// pointer mode fires a `MouseMotion` event continuously while the remote is
    /// moving, and each one otherwise forced a full-frame redraw regardless of
    /// whether the pointer was still over the same card (see `main.rs`'s dirty
    /// tracking).
    /// Deliberately does NOT move `home_focus`/`settings_focused` — that's the
    /// outline+zoom "focused element" state, and moving it on every hover (the
    /// previous behavior) popped rows/cards in and out of that treatment just from
    /// the pointer drifting across the screen. Only keyboard/remote navigation or a
    /// click (`handle_mouse_click` below) moves it now. Hover still drives the
    /// close (X) button's highlight, a conventional affordance this excludes.
    pub fn handle_mouse_motion(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> bool {
        match self.screen {
            Screen::Home => {
                // Home has no close button, but `hover_close` is only ever set by
                // the modal branches below — without clearing it here, hovering a
                // modal's close button and then backing out to Home left it stuck
                // `true` forever (nothing on Home ever set it back to `false`), so
                // `handle_mouse_click`'s `if self.hover_close { ...; return None }`
                // silently swallowed *every* Home click afterward, no matter where
                // it landed. Not folded into the returned "did anything visibly
                // change" bool — Home never draws a close button, so this has no
                // visual effect of its own.
                self.hover_close = false;
                false
            }
            Screen::Settings => {
                let (card, _content) = Self::settings_layout(screen_w, screen_h);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            // Pairing/AddHost/Wake/ForgetHost are plain single-card modals with
            // nothing but the close button to hover-test (unlike Settings
            // above, which also tracks per-row hover) — same one-liner for
            // all four, just a different card rect.
            Screen::Pairing | Screen::AddHost | Screen::Wake | Screen::ForgetHost => {
                let card = match self.screen {
                    Screen::Pairing => Self::pairing_card_rect(screen_w, screen_h),
                    Screen::AddHost => Self::add_host_card_rect(screen_w, screen_h),
                    Screen::Wake => Self::wake_card_rect(screen_w, screen_h),
                    Screen::ForgetHost => Self::forget_host_card_rect(screen_w, screen_h),
                    Screen::Home | Screen::Settings => unreachable!(),
                };
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
        }
    }

    /// Updates `hover_close` and reports whether it actually changed — every modal
    /// screen's close-button hover check in `handle_mouse_motion` follows this same
    /// shape.
    fn set_hover_close(&mut self, hover_close: bool) -> bool {
        let changed = hover_close != self.hover_close;
        self.hover_close = hover_close;
        changed
    }

    /// A pointer click confirms whatever's currently hovered/focused, or triggers
    /// Back if the modal's close (X) button itself is what's hovered.
    pub fn handle_mouse_click(
        &mut self,
        x: i32,
        y: i32,
        screen_w: u32,
        screen_h: u32,
        log: &mut std::fs::File,
    ) -> Option<ConnectTarget> {
        // Re-sync the close-button hover to the click's own position first — a
        // MouseButtonDown can carry a slightly different (x, y) than the last
        // MouseMotion (the physical button press can jostle the remote a little).
        self.handle_mouse_motion(x, y, screen_w, screen_h);
        if self.hover_close {
            // Same "what Back means here" as everywhere else — see `back`'s docs.
            return self.back(log);
        }
        // Unlike hover, a click DOES move `home_focus`/`settings_focused` — fresh at
        // the click's own position, so it confirms what was actually clicked rather
        // than whatever the keyboard/remote last focused elsewhere.
        match self.screen {
            Screen::Home => {
                if let Some(idx) = ui::hit_test_sidebar_row(x, y, self.sidebar_len(), screen_h) {
                    self.home_focus = HomeFocus::Sidebar(idx);
                } else {
                    let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
                    let columns = ui::grid_columns(available_w);
                    // Clicked empty space (`?`'s early `None`) — nothing to focus or confirm.
                    let idx =
                        ui::hit_test_grid_card(x, y, columns, self.grid_len(), ui::SIDEBAR_W as i32, available_w)?;
                    self.home_focus = HomeFocus::Grid(idx);
                }
                self.handle_home_event(MenuEvent::Confirm, screen_w, log)
            }
            Screen::Settings => {
                // An open dropdown has no row grid of its own here — Confirm picks
                // whatever option `dd.focused` (moved by keyboard/remote only, same
                // as everywhere else) already points at; unaffected by this change.
                if self.dropdown.is_none() {
                    let (_, content) = Self::settings_layout(screen_w, screen_h);
                    // Clicked empty space within the card (`?`'s early `None`) — nothing to
                    // focus or confirm.
                    let i = (0..ui::SETTINGS_ROW_COUNT).find(|&i| {
                        let row_y = content.y() + i as i32 * (ui::SETTINGS_ROW_H as i32 + ui::SETTINGS_ROW_GAP);
                        Rect::new(content.x(), row_y, content.width(), ui::SETTINGS_ROW_H).contains_point((x, y))
                    })?;
                    self.settings_focused = i;
                }
                self.handle_settings_event(MenuEvent::Confirm);
                None
            }
            Screen::Pairing | Screen::AddHost => None,
            Screen::Wake => {
                self.handle_wake_event(MenuEvent::Confirm, log);
                None
            }
            Screen::ForgetHost => {
                self.handle_forget_host_event(MenuEvent::Confirm);
                None
            }
        }
    }

    // --------------------------------------------------------------- render --

    /// Renders the current screen. On a Home-screen frame, `render_home` draws
    /// straight into `painter` exactly as it always has — no caching layer
    /// involved, so this costs exactly what it used to. On any modal screen
    /// (Settings/Pairing/AddHost/Wake), Home is only actually re-rendered (into
    /// the cached `home_layer`) when `home_dirty` is set; every other modal-only
    /// frame (hovering a row, opening a dropdown, navigating it) just blits the
    /// cached layer instead of re-drawing the entire sidebar+grid from scratch.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        if matches!(self.screen, Screen::Home) {
            // A Home frame proves what the *current* true state looks like, but
            // that hasn't been captured into `home_layer` (this draw bypassed it
            // entirely) — leave the cache marked stale so the first modal frame
            // afterward re-renders it once before trusting it.
            self.home_dirty = true;
            painter.clear(ui::BG);
            self.render_home(
                painter, text_cache, font_label, font_value, font_title, icon_font, screen_w, screen_h,
            )?;
        } else {
            let mut layer = self
                .home_layer
                .take()
                .unwrap_or_else(|| Painter::new(screen_w, screen_h));
            if self.home_dirty {
                layer.clear(ui::BG);
                self.render_home(
                    &mut layer, text_cache, font_label, font_value, font_title, icon_font, screen_w, screen_h,
                )?;
                self.home_dirty = false;
            }
            // `blit_layer` below overwrites every pixel unconditionally, making a
            // `clear()` here redundant work — skip it.
            painter.blit_layer(&layer);
            self.home_layer = Some(layer);
        }

        match self.screen {
            Screen::Home => {}
            Screen::Pairing => {
                self.render_pairing(
                    painter, text_cache, font_label, font_title, icon_font, screen_w, screen_h,
                )?;
            }
            Screen::Settings => {
                self.render_settings(
                    painter, text_cache, font_label, font_value, icon_font, screen_w, screen_h,
                )?;
            }
            Screen::AddHost => self.render_add_host(
                painter, text_cache, font_label, font_value, font_title, icon_font, screen_w, screen_h,
            )?,
            Screen::Wake => self.render_wake(
                painter, text_cache, font_label, font_title, icon_font, screen_w, screen_h,
            )?,
            Screen::ForgetHost => self.render_forget_host(
                painter, text_cache, font_label, font_value, icon_font, screen_w, screen_h,
            )?,
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_home(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let sidebar_focus = match self.home_focus {
            HomeFocus::Sidebar(i) => Some(i),
            HomeFocus::Grid(_) => None,
        };
        ui::draw_sidebar(
            painter,
            text_cache,
            font_label,
            font_title,
            icon_font,
            &self.entries,
            sidebar_focus,
            screen_h,
        )?;

        let grid_x = ui::SIDEBAR_W as i32;
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        if self.selected_host.is_none() {
            ui::draw_text(
                painter,
                text_cache,
                font_label,
                "No host selected — pick one from the list, or add one.",
                grid_x + ui::GRID_PAD,
                ui::GRID_TOP_Y,
                ui::MUTED,
            )?;
            return Ok(());
        }
        if let Some(status) = &self.home_status {
            // Pinned to the bottom of the panel, not `GRID_TOP_Y` — that sat right on
            // top of the "Desktop" card (`grid_card_rect(0, ...)` starts at the same
            // y), so a connection status/error overlapped the grid instead of sitting
            // clear of it.
            let max_w = available_w.saturating_sub(2 * ui::GRID_PAD as u32);
            let line_h = font_label.height() + 6;
            let line_count = ui::wrap_text(font_label, status, max_w).len() as i32;
            let block_y = screen_h as i32 - ui::GRID_PAD - line_h * line_count;
            ui::draw_text_wrapped(
                painter,
                text_cache,
                font_label,
                status,
                grid_x + ui::GRID_PAD,
                block_y,
                max_w,
                ui::MUTED,
                6,
            )?;
        }
        let columns = ui::grid_columns(available_w);
        let grid_focus = match self.home_focus {
            HomeFocus::Grid(i) => Some(i),
            HomeFocus::Sidebar(_) => None,
        };
        // Card 0 is always "Desktop" (a plain session, no game launch) — never has
        // fetched art of its own.
        let desktop_rect = ui::grid_card_rect(0, columns, grid_x, available_w);
        ui::draw_poster_card(
            painter,
            text_cache,
            font_title,
            font_value,
            desktop_rect,
            "Desktop",
            None,
            grid_focus == Some(0),
        )?;
        for (i, game) in self.games.iter().enumerate() {
            let idx = i + 1;
            let rect = ui::grid_card_rect(idx, columns, grid_x, available_w);
            ui::draw_poster_card(
                painter,
                text_cache,
                font_title,
                font_value,
                rect,
                &game.title,
                self.art.get(&game.id),
                grid_focus == Some(idx),
            )?;
        }
        Ok(())
    }

    /// Shared modal chrome — dark backdrop, the rounded card, and its close (X)
    /// button — every Settings/Pairing/AddHost/Wake screen draws exactly this
    /// before its own content inside `card`.
    fn draw_modal_shell(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        icon_font: &sdl2::ttf::Font,
        card: Rect,
    ) -> Result<()> {
        ui::draw_modal_backdrop(painter);
        ui::draw_modal_card(painter, card);
        ui::draw_icon(
            painter,
            text_cache,
            icon_font,
            ui::modal_close_rect(card),
            ui::ICON_CLOSE,
            if self.hover_close { ui::WHITE } else { ui::MUTED },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_pairing(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = Self::pairing_card_rect(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, icon_font, card)?;

        // Title in `font_label`, matching the other modals — `font_title` is used
        // below for the PIN digits themselves, at a deliberately large display style.
        let after_subtitle_y = ui::draw_modal_header(
            painter,
            text_cache,
            font_label,
            font_label,
            card,
            "Pair with host",
            ui::WHITE,
            "Enter the PIN from the host's pairing dialog.",
            ui::MUTED,
        )?;

        // A clear, generous gap below the subtitle — the PIN entry is this
        // screen's whole point, and it read as cramped sitting right under
        // the subtitle text.
        let digit_h = 80u32;
        let digit_y = after_subtitle_y + 38;
        let digit_w = 64i32;
        let digit_gap = 14i32;
        let total_w = 4 * digit_w + 3 * digit_gap;
        let start_x = card.x() + (card.width() as i32 - total_w) / 2;
        for (i, digit) in self.pin_digits.iter().enumerate() {
            let x = start_x + i as i32 * (digit_w + digit_gap);
            let rect = Rect::new(x, digit_y, digit_w as u32, digit_h);
            let focused = i == self.pin_digit_index;
            let drawn = ui::draw_card(painter, rect, focused);
            let text = digit.to_string();
            let tw = font_title.size_of(&text).map_or(0, |(w, _)| w);
            ui::draw_text(
                painter,
                text_cache,
                font_title,
                &text,
                drawn.x() + (drawn.width() as i32 - tw as i32) / 2,
                drawn.y() + (drawn.height() as i32 - font_title.height()) / 2,
                ui::WHITE,
            )?;
        }
        if let Some(status) = &self.pairing_status {
            let color = if self.pairing_busy { ui::MUTED } else { ui::ERROR_RED };
            // Wrapped, not a single `draw_text` line — a pairing failure's message
            // (whatever the host/network error's own text is) can run longer than a
            // fixed short label and would otherwise overflow the card edge.
            ui::draw_text_wrapped(
                painter,
                text_cache,
                font_label,
                status,
                card.x() + 32,
                digit_y + digit_h as i32 + 32,
                card.width().saturating_sub(64),
                color,
                6,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_settings(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let (card, content) = Self::settings_layout(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, icon_font, card)?;
        ui::draw_text(
            painter,
            text_cache,
            font_label,
            "Settings",
            card.x() + 40,
            card.y() + 36,
            ui::WHITE,
        )?;
        painter.fill_rect(
            Rect::new(card.x() + 40, card.y() + 88, card.width().saturating_sub(80), 1),
            sdl2::pixels::Color::RGBA(0xff, 0xff, 0xff, 0x1e),
        );

        let rows = ui::settings_rows(&self.settings);
        ui::draw_settings_rows(
            painter,
            text_cache,
            font_label,
            font_value,
            icon_font,
            &rows,
            self.settings_focused,
            content,
        )?;

        if self.settings.bitrate_kbps > ui::BITRATE_WARN_KBPS {
            ui::draw_text(
                painter,
                text_cache,
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
            ui::draw_dropdown_overlay(painter, text_cache, font_value, &options, dd.focused, overlay_rect)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_add_host(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = Self::add_host_card_rect(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, icon_font, card)?;

        let after_subtitle_y = ui::draw_modal_header(
            painter,
            text_cache,
            font_label,
            font_value,
            card,
            "Add host",
            ui::WHITE,
            "Enter the host's IP address.",
            ui::MUTED,
        )?;

        let field = Rect::new(
            card.x() + 32,
            after_subtitle_y + 20,
            card.width().saturating_sub(64),
            80,
        );
        let drawn = ui::draw_card(painter, field, true);
        let text_x = drawn.x() + 24;
        let typed = self.add_host.display_text();
        let text_w = font_title.size_of(&typed).map_or(0, |(w, _)| w);
        ui::draw_text(
            painter,
            text_cache,
            font_title,
            &typed,
            text_x,
            drawn.y() + (drawn.height() as i32 - font_title.height()) / 2,
            ui::WHITE,
        )?;
        // A blinkless text-cursor bar right after what's typed so far — there's
        // no fixed-width mask anymore to show *where* editing happens, so this
        // stands in for it.
        let caret = Rect::new(
            text_x + text_w as i32 + 6,
            drawn.y() + 16,
            3,
            drawn.height().saturating_sub(32),
        );
        painter.fill_rect(caret, ui::ACCENT_BRIGHT);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_wake(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let Some(wake) = &self.wake else { return Ok(()) };
        let card = Self::wake_card_rect(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, icon_font, card)?;

        let status = if wake.mac.is_empty() {
            format!(
                "Couldn't reach {} — it may be powered off, asleep, or off the network. No \
                 Wake-on-LAN address is on record for it yet, so it can't be woken from here; it'll \
                 reconnect automatically once it's back online.",
                wake.name
            )
        } else if wake.sent {
            format!(
                "Sent a wake signal to {} — waiting for it to come back online…",
                wake.name
            )
        } else {
            format!("Couldn't reach {} — it may be powered off or asleep.", wake.name)
        };
        let after_status_y = ui::draw_modal_header(
            painter, text_cache, font_title, font_label, card, "Host unreachable", ui::WHITE, &status, ui::MUTED,
        )?;

        // No MAC on record — nothing to send or automate, so there's no row to draw
        // (see `handle_wake_event`'s matching guard); the status text above already
        // explains why, and `App::drain_discovery` still reconnects automatically the
        // moment this host reappears on mDNS.
        if !wake.mac.is_empty() {
            let content = Rect::new(
                card.x() + 32,
                after_status_y + 28,
                card.width().saturating_sub(64),
                ui::SETTINGS_ROW_H,
            );
            let send_label = if wake.sent { "Send again" } else { "Send Wake-on-LAN now" };
            ui::draw_wake_rows(
                painter,
                text_cache,
                font_label,
                icon_font,
                content,
                send_label,
                wake.focused,
                self.settings.wol_auto_send,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_forget_host(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let Some(name) = self
            .host_menu_index
            .and_then(|i| self.entries.get(i))
            .map(HostEntry::name)
        else {
            return Ok(());
        };
        let card = Self::forget_host_card_rect(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, icon_font, card)?;

        let after_subtitle_y = ui::draw_modal_header(
            painter,
            text_cache,
            font_label,
            font_value,
            card,
            "Forget this host?",
            ui::WHITE,
            &format!("\"{name}\" will be removed from your host list."),
            ui::MUTED,
        )?;

        let content = Rect::new(card.x() + 32, after_subtitle_y + 32, card.width().saturating_sub(64), 72);
        ui::draw_confirm_buttons(
            painter,
            text_cache,
            font_label,
            icon_font,
            content,
            &[
                ui::ConfirmButton {
                    icon: Some(ui::ICON_DELETE),
                    label: "Forget",
                    color: ui::ERROR_RED,
                },
                ui::ConfirmButton {
                    icon: None,
                    label: "Cancel",
                    color: ui::WHITE,
                },
            ],
            self.host_menu_focused,
        )?;
        Ok(())
    }
}
