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
use crate::compositor::{DrawCmd, Tile};
use crate::store::{self, KnownHost, Settings};
use crate::ui::{self, AddHostState, HostEntry, MenuEvent, Painter};

#[derive(Clone, Copy, PartialEq, Eq)]
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

/// What the pairing modal's input is aimed at: the PIN digit row, or the
/// "Request access" (no-PIN, approve-on-host) button below it. The digit row
/// consumes all four arrows (Left/Right move between digits, Up/Down spin the
/// value), so the button is reached by tabbing Right past the last digit, by the
/// Magic Remote pointer, or by the Secondary shortcut — see `handle_pairing_event`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PairingFocus {
    Pin,
    RequestAccess,
}

/// How long the grid focus pop (card scaling in) runs.
const FOCUS_POP: Duration = Duration::from_millis(140);
/// How long a modal's fade/slide-in runs.
const MODAL_FADE: Duration = Duration::from_millis(170);

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
    /// Whether the pairing modal's input is on the PIN row or the Request-access button.
    pub pairing_focus: PairingFocus,
    pub pairing_status: Option<String>,
    pub pairing_busy: bool,
    /// Index into `entries` currently being paired — captured when entering
    /// `Screen::Pairing`.
    pairing_entry: usize,
    /// Whether the Magic Remote's pointer is currently hovering a modal's
    /// close (X) button.
    pub hover_close: bool,
    identity: (String, String),
    // ------------------------------------------------------------- GPU tiles --
    // Rasterized-once tile sources for the GPU compositor (`compositor.rs`):
    // `prepare_tiles` rebuilds whichever are stale and reports them for upload;
    // `draw_list` then composes each frame from their textures. Focus movement,
    // scrolling, and animations never re-rasterize anything.
    /// Focus-free sidebar strip (`SIDEBAR_W` × screen height): panel, brand
    /// mark + wordmark, every row unfocused. Stale when row content changes
    /// (`sidebar_dirty`), never on focus movement.
    sidebar_layer: Option<Painter>,
    sidebar_dirty: bool,
    /// Per-card tiles (shadow baked in, transparent padding), index-aligned
    /// with the grid. `None` = not yet rasterized (or invalidated).
    card_tiles: Vec<Option<Painter>>,
    /// All card tiles stale (games list / host changed).
    grid_dirty: bool,
    /// Individual card tiles stale (cover art arrived) — cheaper than
    /// `grid_dirty` when the layout is unchanged.
    grid_cards_dirty: Vec<usize>,
    /// The shared focus-ring glow tile (one per card size).
    ring_tile: Option<Painter>,
    /// The focused sidebar row's tile, keyed by row index.
    focused_row_tile: Option<(usize, Painter)>,
    /// The active modal rasterized full-screen (transparent surroundings);
    /// rebuilt on content changes, composited with fade/slide by the GPU.
    modal_tile: Option<Painter>,
    /// Home's status line block, keyed by its text.
    status_tile: Option<(String, Painter)>,
    /// The static "No host selected" hint line.
    nohost_tile: Option<Painter>,
    // ------------------------------------------------------------ animations --
    /// Grid scroll offset actually rendered this frame (px; 0 = row 0 at
    /// `GRID_TOP_Y`) — eases toward `grid_scroll_target` each tick.
    pub grid_scroll: i32,
    grid_scroll_target: i32,
    /// When the current grid-focus pop started (card scales in over
    /// `FOCUS_POP` — set on every d-pad focus move).
    focus_anim: Option<Instant>,
    /// When the open modal's fade/slide-in started (`MODAL_FADE`).
    modal_anim: Option<Instant>,
    /// Last screen `prepare_tiles` saw — a change triggers the modal-open
    /// animation and a modal re-rasterize without every transition site
    /// needing to remember to.
    last_screen: Screen,
    /// In-flight PIN-pairing / request-access ceremony, delivering its outcome
    /// from a background thread — the ceremony blocks for up to minutes
    /// (request-access parks until a human approves it on the host), which used
    /// to freeze the whole UI when run inline on this thread. Drained by
    /// `drain_pairing` each tick; dropping the receiver (Back while busy)
    /// cancels: the worker's send fails and it exits.
    pairing_rx: Option<std::sync::mpsc::Receiver<PairingOutcome>>,
}

/// What a finished background pairing/request-access ceremony reports back —
/// everything needed to persist the host on success (captured going in, so the
/// worker doesn't need `App` access).
struct PairingOutcome {
    host: String,
    port: u16,
    name: String,
    mgmt_port: Option<u16>,
    mac: Vec<String>,
    /// The host's now-verified fingerprint, or a user-displayable error.
    result: Result<[u8; 32], String>,
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
            pairing_focus: PairingFocus::Pin,
            pairing_status: None,
            pairing_busy: false,
            pairing_entry: 0,
            hover_close: false,
            identity,
            sidebar_layer: None,
            sidebar_dirty: true,
            card_tiles: Vec::new(),
            grid_dirty: true,
            grid_cards_dirty: Vec::new(),
            ring_tile: None,
            focused_row_tile: None,
            modal_tile: None,
            status_tile: None,
            nohost_tile: None,
            grid_scroll: 0,
            grid_scroll_target: 0,
            focus_anim: None,
            modal_anim: None,
            last_screen: Screen::Home,
            pairing_rx: None,
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
            self.sidebar_dirty = true;
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
            // Layout is unchanged by art arriving — queue a repaint of just that
            // card's tile (see `grid_cards_dirty`) rather than a full layer rebuild.
            if let Some(i) = self.games.iter().position(|g| g.id == loaded.game_id) {
                self.grid_cards_dirty.push(i + 1); // +1: card 0 is Desktop
            }
            self.art.insert(loaded.game_id, loaded.pixmap);
            changed = true;
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
        screen_h: u32,
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
                        let i = *i;
                        self.ensure_grid_visible(i, columns, screen_w, screen_h);
                    }
                }
            },
            MenuEvent::Down => match &mut self.home_focus {
                HomeFocus::Sidebar(i) => *i = (*i + 1) % sidebar_len,
                HomeFocus::Grid(i) => {
                    let next = *i + columns;
                    if next < grid_len {
                        *i = next;
                        self.ensure_grid_visible(next, columns, screen_w, screen_h);
                    }
                }
            },
            MenuEvent::Left => {
                if let HomeFocus::Grid(i) = self.home_focus {
                    if i % columns == 0 {
                        self.home_focus = HomeFocus::Sidebar(self.sidebar_index_for_selected());
                    } else {
                        self.home_focus = HomeFocus::Grid(i - 1);
                        self.ensure_grid_visible(i - 1, columns, screen_w, screen_h);
                    }
                }
            }
            MenuEvent::Right => match self.home_focus {
                HomeFocus::Sidebar(_) => {
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
            // no-op. (It used to be a shortcut straight to Settings, but that made
            // Back in Settings feel broken: close Settings, press Back again, and
            // Settings popped right back up.)
            Screen::Home => None,
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

    /// The largest useful `grid_scroll` for the current library/layout — 0 when
    /// everything already fits on screen.
    fn max_grid_scroll(&self, columns: usize, available_w: u32, screen_h: u32) -> i32 {
        let viewport_h = screen_h as i32 - ui::GRID_PAD - ui::GRID_TOP_Y;
        (ui::grid_layer_height(self.grid_len(), columns, available_w) as i32
            - 2 * ui::GRID_LAYER_PAD
            - viewport_h)
            .max(0)
    }

    /// Scrolls the grid (via `grid_scroll_target` — the rendered offset eases
    /// toward it, see `tick_animations`) just far enough that focused card `idx`,
    /// including its focus-ring halo, will be fully on screen; also starts the
    /// focus pop, since this is called on exactly the moves that change grid
    /// focus. Clamped to the grid's real extent.
    fn ensure_grid_visible(&mut self, idx: usize, columns: usize, screen_w: u32, screen_h: u32) {
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

    /// Advances every live animation one tick — the eased scroll, the focus pop,
    /// the modal fade — and reports whether anything is still moving (the main
    /// loop keeps rendering while true). Expired animations report one final
    /// `true` so their end state gets drawn.
    pub fn tick_animations(&mut self) -> bool {
        let mut animating = false;
        let d = self.grid_scroll_target - self.grid_scroll;
        if d != 0 {
            // Exponential ease-out: cover ~35% of the remaining distance per
            // tick, snapping when close so it terminates.
            let step = if d.abs() <= 3 {
                d
            } else {
                let s = (f64::from(d) * 0.35) as i32;
                if s == 0 {
                    d.signum()
                } else {
                    s
                }
            };
            self.grid_scroll += step;
            animating = true;
        }
        if let Some(t) = self.focus_anim {
            if t.elapsed() >= FOCUS_POP {
                self.focus_anim = None;
            }
            animating = true;
        }
        if let Some(t) = self.modal_anim {
            if t.elapsed() >= MODAL_FADE {
                self.modal_anim = None;
            }
            animating = true;
        }
        animating
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
                self.pairing_focus = PairingFocus::Pin;
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
        self.grid_dirty = true;
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
        // Only claim "sent" if a magic packet actually went out — `wake_and_log`
        // returns false on an unparseable MAC / no usable interface, and showing
        // "Sent a wake signal… waiting" for a packet that never left would leave
        // the user waiting on nothing.
        let sent = crate::wol::wake_and_log(&wake.mac, wake.host.parse().ok(), &wake.name, log);
        let now = Instant::now();
        if sent {
            wake.sent = true;
            wake.since.get_or_insert(now);
        } else {
            wake.reason = "Couldn't send a wake signal (no usable MAC/interface).".into();
        }
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
        self.sidebar_dirty = true;
        self.grid_dirty = true;
    }

    /// Handles one menu event on the pairing modal. Two focus zones (`PairingFocus`):
    /// the PIN digit row (blocking SPAKE2 ceremony on `Confirm`) and the "Request
    /// access" button (blocking no-PIN, park-until-approved connect on `Confirm`).
    pub fn handle_pairing_event(&mut self, ev: MenuEvent, log: &mut std::fs::File) {
        if self.pairing_busy {
            // Mid-ceremony, Back cancels (dropping the receiver orphans the
            // worker — its send fails and it exits); everything else is ignored.
            if ev == MenuEvent::Back {
                self.pairing_rx = None;
                self.pairing_busy = false;
                self.pairing_status = None;
                self.screen = Screen::Home;
            }
            return;
        }
        // Back always leaves the modal; Secondary is the "switch pairing method"
        // shortcut — both work from either focus zone.
        match ev {
            MenuEvent::Back => {
                self.screen = Screen::Home;
                return;
            }
            MenuEvent::Secondary => {
                self.pairing_focus = match self.pairing_focus {
                    PairingFocus::Pin => PairingFocus::RequestAccess,
                    PairingFocus::RequestAccess => PairingFocus::Pin,
                };
                return;
            }
            _ => {}
        }
        match self.pairing_focus {
            // The digits sit in a horizontal row: Left/Right move *between* them and
            // Up/Down spin the focused digit's *value* (odometer-style: Up = +1, Down =
            // −1, wrapping 0..=9). Tabbing Right off the last digit drops focus onto the
            // "Request access" button below; `Confirm` submits the PIN.
            PairingFocus::Pin => match ev {
                MenuEvent::Up => {
                    self.pin_digits[self.pin_digit_index] = (self.pin_digits[self.pin_digit_index] + 1) % 10;
                }
                MenuEvent::Down => {
                    self.pin_digits[self.pin_digit_index] = (self.pin_digits[self.pin_digit_index] + 9) % 10;
                }
                MenuEvent::Left => {
                    if self.pin_digit_index > 0 {
                        self.pin_digit_index -= 1;
                    }
                }
                MenuEvent::Right => {
                    if self.pin_digit_index + 1 < self.pin_digits.len() {
                        self.pin_digit_index += 1;
                    } else {
                        self.pairing_focus = PairingFocus::RequestAccess;
                    }
                }
                MenuEvent::Confirm => self.try_pair(log),
                MenuEvent::Back | MenuEvent::Secondary => {} // handled above
            },
            // Left tabs back onto the PIN row; Confirm sends the access request.
            PairingFocus::RequestAccess => match ev {
                MenuEvent::Left => self.pairing_focus = PairingFocus::Pin,
                MenuEvent::Confirm => self.try_request_access(log),
                // Up/Down/Right are no-ops here; Back/Secondary were handled above.
                MenuEvent::Up | MenuEvent::Down | MenuEvent::Right | MenuEvent::Back | MenuEvent::Secondary => {}
            },
        }
    }

    /// The no-PIN pairing path: connect trust-on-first-use (presenting our identity so
    /// the host operator sees this device), which the host PARKS until the operator
    /// approves it in the host UI, then pin the now-verified fingerprint and land on the
    /// host's game grid — the same success path as `try_pair`, and likewise run on a
    /// background thread (this one can block for MINUTES waiting on the approval, so
    /// freezing the UI for it is not an option). The 185s budget matches `run_inner`'s
    /// pending-approval wait, long enough for a human to notice and click.
    fn try_request_access(&mut self, log: &mut std::fs::File) {
        let entry = &self.entries[self.pairing_entry];
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        let mgmt_port = entry.mgmt_port();
        let mac = entry.mac().to_vec();
        self.pairing_busy = true;
        self.pairing_status = Some("Requesting access… approve this device on the host".into());
        let _ = writeln!(log, "requesting access to {host}:{port}");

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.pairing_rx = Some(rx);
        std::thread::spawn(move || {
            let result =
                crate::session::request_access(&host, port, identity, std::time::Duration::from_secs(185))
                    .map_err(|e| format!("Request failed: {e}"));
            let _ = tx.send(PairingOutcome {
                host,
                port,
                name,
                mgmt_port,
                mac,
                result,
            });
        });
    }

    /// Drains a finished background pairing/request-access ceremony, if any —
    /// called each tick from `run_ui_flow` like the other `drain_*`s. Success
    /// persists the host and lands on its game grid; failure re-arms the pairing
    /// modal with the error text.
    pub fn drain_pairing(&mut self, log: &mut std::fs::File) -> bool {
        let Some(rx) = &self.pairing_rx else { return false };
        let Ok(outcome) = rx.try_recv() else { return false };
        self.pairing_rx = None;
        self.pairing_busy = false;
        match outcome.result {
            Ok(fingerprint) => {
                let _ = writeln!(log, "paired ok ({}:{}), fingerprint set", outcome.host, outcome.port);
                store::upsert_known_host(
                    &mut self.known_hosts,
                    KnownHost {
                        name: outcome.name,
                        host: outcome.host.clone(),
                        port: outcome.port,
                        fingerprint: Some(fingerprint),
                        mgmt_port: outcome.mgmt_port,
                        mac: outcome.mac,
                    },
                );
                let _ = store::save_known_hosts(&self.known_hosts);
                self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
                self.sidebar_dirty = true;
                self.screen = Screen::Home;
                self.select_host(outcome.host, outcome.port, outcome.mgmt_port, log);
            }
            Err(e) => {
                let _ = writeln!(log, "pairing/request failed: {e}");
                self.pairing_status = Some(e);
            }
        }
        true
    }

    /// Direct digit entry (the Magic Remote's number buttons) — types `digit` into
    /// the current PIN slot and auto-advances, like a phone lock-screen PIN pad,
    /// instead of requiring left/right cycling through 0-9 per digit.
    pub fn enter_pin_digit(&mut self, digit: u8, log: &mut std::fs::File) {
        if self.pairing_busy {
            return;
        }
        // A typed digit is unambiguously PIN input — pull focus back off the
        // Request-access button so it lands in the digit row (and can't
        // accidentally auto-submit the no-PIN path instead).
        self.pairing_focus = PairingFocus::Pin;
        self.pin_digits[self.pin_digit_index] = digit;
        if self.pin_digit_index + 1 < self.pin_digits.len() {
            self.pin_digit_index += 1;
        } else {
            self.try_pair(log);
        }
    }

    /// Starts the PIN pairing ceremony on a background thread (see
    /// `pairing_rx`'s docs — the ceremony blocks, the UI must not).
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

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.pairing_rx = Some(rx);
        std::thread::spawn(move || {
            let result = punktfunk_core::client::NativeClient::pair(
                &host,
                port,
                (&identity.0, &identity.1),
                &pin,
                "webOS TV",
                std::time::Duration::from_secs(30),
            )
            .map_err(|e| format!("Pairing failed: {e}"));
            // Send failing just means the user backed out and the receiver is
            // gone — nothing to deliver to.
            let _ = tx.send(PairingOutcome {
                host,
                port,
                name,
                mgmt_port,
                mac,
                result,
            });
        });
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

    /// The "Request access" button's rect — anchored to the card bottom (not to the
    /// dynamic PIN-row position) so `render_pairing` and `handle_mouse_click` compute
    /// the exact same rect without threading the header's measured height between them.
    fn pairing_request_button_rect(card: Rect) -> Rect {
        let margin = 40i32;
        let h = 52u32;
        let y = card.y() + card.height() as i32 - h as i32 - 26;
        Rect::new(card.x() + margin, y, card.width().saturating_sub(margin as u32 * 2), h)
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
                    let idx = ui::hit_test_grid_card(
                        x,
                        y,
                        columns,
                        self.grid_len(),
                        ui::SIDEBAR_W as i32,
                        available_w,
                        self.grid_scroll,
                    )?;
                    self.home_focus = HomeFocus::Grid(idx);
                }
                self.handle_home_event(MenuEvent::Confirm, screen_w, screen_h, log)
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
            Screen::Pairing => {
                // The Magic Remote pointer is the most reliable input on this TV, so the
                // "Request access" button is clickable directly: focus it and confirm.
                let card = Self::pairing_card_rect(screen_w, screen_h);
                if Self::pairing_request_button_rect(card).contains_point((x, y)) {
                    self.pairing_focus = PairingFocus::RequestAccess;
                    self.handle_pairing_event(MenuEvent::Confirm, log);
                }
                None
            }
            Screen::AddHost => None,
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

    /// The title of grid card `idx` (0 = the fixed "Desktop" card) and its cover
    /// art, if fetched.
    fn grid_card_content(&self, idx: usize) -> (&str, Option<&Pixmap>) {
        if idx == 0 {
            ("Desktop", None)
        } else {
            let game = &self.games[idx - 1];
            (game.title.as_str(), self.art.get(&game.id))
        }
    }

    /// Cubic ease-out for the animation fractions below.
    fn ease(f: f32) -> f32 {
        1.0 - (1.0 - f).powi(3)
    }

    /// Eased 0..=1 progress of an animation started at `t`; 1.0 when done/absent.
    fn anim_frac(anim: Option<Instant>, dur: Duration) -> f32 {
        match anim {
            Some(t) => Self::ease((t.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0)),
            None => 1.0,
        }
    }

    /// Rasterizes every stale tile (tiny-skia, CPU — the only place rasterization
    /// happens) and returns which tiles need their GPU texture re-uploaded.
    /// `content_dirty` is the main loop's "an event/drain changed something this
    /// tick" flag — it forces the open modal's tile to re-rasterize, since modal
    /// content has no finer dirty tracking of its own. Pure animation frames pass
    /// `false` and rasterize nothing at all.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_tiles(
        &mut self,
        text_cache: &mut crate::ui::TextCache,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        screen_w: u32,
        screen_h: u32,
        content_dirty: bool,
    ) -> Result<Vec<Tile>> {
        let mut updated = Vec::new();
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);
        let (card_w, card_h) = ui::grid_card_size(available_w, columns);

        // Screen transitions are detected centrally here (rather than at every
        // `self.screen = ...` site): opening a modal starts its fade-in and
        // forces its first rasterize.
        let screen_changed = self.screen != self.last_screen;
        if screen_changed {
            self.last_screen = self.screen;
            if !matches!(self.screen, Screen::Home) {
                self.modal_anim = Some(Instant::now());
            }
        }

        if self.sidebar_dirty || self.sidebar_layer.is_none() {
            let mut layer = match self.sidebar_layer.take() {
                Some(l) => l,
                None => Painter::new(ui::SIDEBAR_W, screen_h),
            };
            ui::draw_sidebar(
                &mut layer,
                text_cache,
                font_label,
                font_value,
                icon_font,
                &self.entries,
                None,
                screen_h,
            )?;
            self.sidebar_layer = Some(layer);
            self.sidebar_dirty = false;
            self.focused_row_tile = None; // row content may have changed under it
            updated.push(Tile::Sidebar);
        }
        if let HomeFocus::Sidebar(i) = self.home_focus {
            let stale = !matches!(&self.focused_row_tile, Some((idx, _)) if *idx == i);
            if stale {
                let tile = ui::render_focused_row_tile(text_cache, font_label, icon_font, &self.entries, i)?;
                self.focused_row_tile = Some((i, tile));
                updated.push(Tile::FocusRow);
            }
        }

        if self.selected_host.is_some() {
            let count = self.grid_len();
            if self.grid_dirty || self.card_tiles.len() != count {
                self.card_tiles = std::iter::repeat_with(|| None).take(count).collect();
                self.grid_dirty = false;
                self.grid_cards_dirty.clear();
            } else {
                for idx in std::mem::take(&mut self.grid_cards_dirty) {
                    if idx < count {
                        self.card_tiles[idx] = None;
                    }
                }
            }
            for idx in 0..count {
                if self.card_tiles[idx].is_none() {
                    let tile = {
                        let (title, art) = self.grid_card_content(idx);
                        ui::render_card_tile(text_cache, font_title, font_value, card_w, card_h, title, art)?
                    };
                    self.card_tiles[idx] = Some(tile);
                    updated.push(Tile::Card(idx));
                }
            }
            let ring_w = card_w + 2 * ui::FOCUS_RING_PAD as u32;
            if !matches!(&self.ring_tile, Some(p) if p.width() == ring_w) {
                self.ring_tile = Some(ui::render_focus_ring_tile(card_w, card_h));
                updated.push(Tile::Ring);
            }
            match &self.home_status {
                Some(s) => {
                    let stale = !matches!(&self.status_tile, Some((t, _)) if t == s);
                    if stale {
                        let max_w = available_w.saturating_sub(2 * ui::GRID_PAD as u32);
                        let tile = ui::render_wrapped_text_tile(text_cache, font_label, s, max_w, ui::MUTED, 6)?;
                        self.status_tile = Some((s.clone(), tile));
                        updated.push(Tile::Status);
                    }
                }
                None => self.status_tile = None,
            }
        } else if self.nohost_tile.is_none() {
            self.nohost_tile = Some(ui::render_text_tile(
                text_cache,
                font_label,
                "No host selected — pick one from the list, or add one.",
                ui::MUTED,
            )?);
            updated.push(Tile::NoHost);
        }

        let modal_open = !matches!(self.screen, Screen::Home);
        if modal_open && (screen_changed || content_dirty || self.modal_tile.is_none()) {
            let mut p = match self.modal_tile.take() {
                Some(p) => p,
                None => Painter::new(screen_w, screen_h),
            };
            p.clear_transparent();
            match self.screen {
                Screen::Home => unreachable!("modal_open checked above"),
                Screen::Pairing => {
                    self.render_pairing(&mut p, text_cache, font_label, font_title, icon_font, screen_w, screen_h)?;
                }
                Screen::Settings => {
                    self.render_settings(&mut p, text_cache, font_label, font_value, icon_font, screen_w, screen_h)?;
                }
                Screen::AddHost => self.render_add_host(
                    &mut p, text_cache, font_label, font_value, font_title, icon_font, screen_w, screen_h,
                )?,
                Screen::Wake => {
                    self.render_wake(&mut p, text_cache, font_label, font_title, icon_font, screen_w, screen_h)?;
                }
                Screen::ForgetHost => {
                    self.render_forget_host(&mut p, text_cache, font_label, font_value, icon_font, screen_w, screen_h)?;
                }
            }
            self.modal_tile = Some(p);
            updated.push(Tile::Modal);
        }
        Ok(updated)
    }

    /// The pixmap behind `tile`, for the compositor to upload.
    pub fn tile_pixmap(&self, tile: Tile) -> Option<&Painter> {
        match tile {
            Tile::Sidebar => self.sidebar_layer.as_ref(),
            Tile::FocusRow => self.focused_row_tile.as_ref().map(|(_, p)| p),
            Tile::Card(i) => self.card_tiles.get(i).and_then(|t| t.as_ref()),
            Tile::Ring => self.ring_tile.as_ref(),
            Tile::Modal => self.modal_tile.as_ref(),
            Tile::Status => self.status_tile.as_ref().map(|(_, p)| p),
            Tile::NoHost => self.nohost_tile.as_ref(),
            // Stream-side only (uploaded directly by `run_inner`'s overlay
            // refresh) — never one of App's menu tiles.
            Tile::StatsOverlay => None,
        }
    }

    /// Builds this frame's draw list (paint order) from the current state and
    /// animation clocks — pure bookkeeping, no rasterization. The GPU executes it
    /// (`Compositor::execute`).
    pub fn draw_list(&self, screen_w: u32, screen_h: u32) -> Vec<DrawCmd> {
        let mut cmds = Vec::new();
        let grid_x = ui::SIDEBAR_W as i32;
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);

        cmds.push(DrawCmd::Tex {
            tile: Tile::Sidebar,
            dst: Rect::new(0, 0, ui::SIDEBAR_W, screen_h),
            alpha: 0xff,
        });

        if self.selected_host.is_none() {
            if let Some(p) = &self.nohost_tile {
                cmds.push(DrawCmd::Tex {
                    tile: Tile::NoHost,
                    dst: Rect::new(grid_x + ui::GRID_PAD, ui::GRID_TOP_Y, p.width(), p.height()),
                    alpha: 0xff,
                });
            }
        } else {
            let count = self.grid_len();
            let focused = match self.home_focus {
                HomeFocus::Grid(i) if i < count => Some(i),
                HomeFocus::Grid(_) | HomeFocus::Sidebar(_) => None,
            };
            let pad = ui::CARD_TILE_PAD;
            for idx in 0..count {
                if Some(idx) == focused {
                    continue; // drawn last, on top of its neighbors
                }
                let r = ui::grid_card_rect(idx, columns, grid_x, available_w);
                let y = r.y() - self.grid_scroll;
                if y + r.height() as i32 + pad < 0 || y - pad > screen_h as i32 {
                    continue; // culled — fully off-screen at this scroll offset
                }
                cmds.push(DrawCmd::Tex {
                    tile: Tile::Card(idx),
                    dst: Rect::new(r.x() - pad, y - pad, r.width() + 2 * pad as u32, r.height() + 2 * pad as u32),
                    alpha: 0xff,
                });
            }
            if let Some(idx) = focused {
                // The focus pop: the GPU scales the (unfocused) card tile up
                // around its center as the pop progresses, with the shared ring
                // tile fading in over it at the same scale.
                let f = Self::anim_frac(self.focus_anim, FOCUS_POP);
                let scale = 1.0 + 0.028 * f;
                let r = ui::grid_card_rect(idx, columns, grid_x, available_w);
                let y = r.y() - self.grid_scroll;
                let cx = r.x() as f32 + r.width() as f32 / 2.0;
                let cy = y as f32 + r.height() as f32 / 2.0;
                let tw = (r.width() + 2 * pad as u32) as f32 * scale;
                let th = (r.height() + 2 * pad as u32) as f32 * scale;
                cmds.push(DrawCmd::Tex {
                    tile: Tile::Card(idx),
                    dst: Rect::new((cx - tw / 2.0) as i32, (cy - th / 2.0) as i32, tw as u32, th as u32),
                    alpha: 0xff,
                });
                let rp = ui::FOCUS_RING_PAD;
                let rw = (r.width() + 2 * rp as u32) as f32 * scale;
                let rh = (r.height() + 2 * rp as u32) as f32 * scale;
                cmds.push(DrawCmd::Tex {
                    tile: Tile::Ring,
                    dst: Rect::new((cx - rw / 2.0) as i32, (cy - rh / 2.0) as i32, rw as u32, rh as u32),
                    alpha: (255.0 * f) as u8,
                });
            }
            if self.home_status.is_some() {
                if let Some((_, p)) = &self.status_tile {
                    let y = screen_h as i32 - ui::GRID_PAD - p.height() as i32;
                    cmds.push(DrawCmd::Tex {
                        tile: Tile::Status,
                        dst: Rect::new(grid_x + ui::GRID_PAD, y, p.width(), p.height()),
                        alpha: 0xff,
                    });
                }
            }
        }

        if let HomeFocus::Sidebar(i) = self.home_focus {
            let rect = if i == self.entries.len() + 1 {
                ui::settings_row_rect(screen_h)
            } else {
                ui::sidebar_row_rect(i)
            };
            let pad = ui::ROW_TILE_PAD;
            cmds.push(DrawCmd::Tex {
                tile: Tile::FocusRow,
                dst: Rect::new(rect.x() - pad, rect.y() - pad, rect.width() + 2 * pad as u32, rect.height() + 2 * pad as u32),
                alpha: 0xff,
            });
        }

        if !matches!(self.screen, Screen::Home) {
            // Modal open: the scrim fades in and the modal slides up its last
            // ~26px while fading — both pure GPU parameters.
            let m = Self::anim_frac(self.modal_anim, MODAL_FADE);
            cmds.push(DrawCmd::Fill {
                rect: Rect::new(0, 0, screen_w, screen_h),
                color: sdl2::pixels::Color::RGBA(0, 0, 0, (f32::from(ui::MODAL_SCRIM.a) * m) as u8),
            });
            let dy = ((1.0 - m) * 26.0) as i32;
            cmds.push(DrawCmd::Tex {
                tile: Tile::Modal,
                dst: Rect::new(0, dy, screen_w, screen_h),
                alpha: (255.0 * m) as u8,
            });
        }
        cmds
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
        // No backdrop here: the scrim behind the modal is a GPU fill in
        // `draw_list` (it fades in with the modal), and this painter is the
        // modal's own transparent tile, not the composed frame.
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
            "Enter the host's PIN, or request access and approve on the host.",
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
            // Only highlight a digit while the PIN row actually has focus — when focus is
            // on the Request-access button, no digit should read as selected.
            let focused = i == self.pin_digit_index && self.pairing_focus == PairingFocus::Pin;
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
                digit_y + digit_h as i32 + 24,
                card.width().saturating_sub(64),
                color,
                6,
            )?;
        }

        // The no-PIN "Request access" button, anchored to the card bottom.
        let btn = Self::pairing_request_button_rect(card);
        let btn_focused = self.pairing_focus == PairingFocus::RequestAccess;
        let drawn = ui::draw_card(painter, btn, btn_focused);
        let label = "Request access";
        let lw = font_label.size_of(label).map_or(0, |(w, _)| w);
        ui::draw_text(
            painter,
            text_cache,
            font_label,
            label,
            drawn.x() + (drawn.width() as i32 - lw as i32) / 2,
            drawn.y() + (drawn.height() as i32 - font_label.height()) / 2,
            if btn_focused { ui::WHITE } else { ui::MUTED },
        )?;
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
