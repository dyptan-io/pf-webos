//! The pre-stream UI flow: a persistent Home screen (sidebar of known hosts +
//! detail grid of the selected host's games) with Pairing/Settings/Add-host as
//! centered modals on top of it — modeled on moonlight-tv's actual layout (see
//! `ui.rs`'s module docs). `ui.rs` owns drawing/input-mapping primitives,
//! `store.rs` owns persistence, `discovery.rs` owns mDNS.
use std::time::{Duration, Instant};

use anyhow::Result;
use sdl2::rect::Rect;
use tiny_skia::Pixmap;

use crate::compositor::{DrawCmd, Tile};
use crate::library::GameEntry;
use crate::store::{self, KnownHost, Settings};
use crate::ui::{self, AddHostState, HostEntry, MenuEvent, Painter};

mod about;
mod addhost;
mod edithost;
mod forget;
mod home;
mod hostmenu;
mod pairing;
mod reach;
mod settings;
mod speedtest;
mod wake;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Screen {
    Home,
    Pairing,
    Settings,
    AddHost,
    /// "This configured host is unreachable — send it a Wake-on-LAN signal?" — see
    /// `WakeState`'s docs.
    Wake,
    /// "Forget this host?" — a centered Forget/Cancel confirmation, reached from
    /// `HostMenu`. Which host is `App::host_menu_index`.
    ForgetHost,
    /// Per-host actions (connect / pair / speed test / wake / edit / forget), opened
    /// by a long-press of OK on a sidebar host row (see `main.rs`'s hold-timer).
    /// This is the extension point for anything new that acts on one host — adding
    /// an action is one row in `App::host_menu_rows` plus one arm in
    /// `App::confirm_host_menu_row`. Which host is `App::host_menu_index`.
    HostMenu,
    /// Editing an existing host's address (`HostMenu` -> "Edit host"), sharing the
    /// add-host entry widget. Which host is `App::edit_host_index`.
    EditHost,
    /// About & licenses — the build version, this app's licence, and the generated
    /// third-party notices. Reached from Settings.
    About,
    /// Per-host network speed test (`HostMenu` -> "Test network speed"), measuring over
    /// the real data plane. See `app::speedtest`.
    SpeedTest,
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

/// Rows of cards kept rasterized beyond the viewport in each direction. Small enough to
/// bound memory, large enough that a d-pad row step or a wheel notch lands on a card that
/// is already built.
const CARD_PREFETCH_ROWS: i32 = 2;
/// Rows beyond which a built tile is dropped. Deliberately larger than
/// `CARD_PREFETCH_ROWS` — the hysteresis is what stops a card oscillating between built
/// and evicted when the scroll position sits right on a boundary.
const CARD_KEEP_ROWS: i32 = 5;
/// Card tiles rasterized *and uploaded* per frame.
///
/// This is the fix for a large library freezing the client. The previous code built every
/// tile in the library in a single pass the moment the grid went dirty: at 1080p a card is
/// ~260x346, so a 365-title library meant ~366 tiny-skia rasterizations and ~366 GPU
/// texture uploads back to back, plus ~169 MB of `Pixmap` retained — in a 32-bit process
/// on a TV with under 2 GB usable. Spreading the work keeps every frame bounded; cards
/// outside the window are simply not drawn (`Compositor::execute` skips a tile with no
/// texture), so they appear as they are built rather than blocking the UI.
///
/// Only bounds the visible+prefetch window (a few dozen tiles), not the whole library, so
/// it can be much higher than the original whole-library concern without reintroducing
/// the freeze.
///
/// Lowered from 2 to 1 after on-device telemetry (`TELEMETRY=auto`) during the loading
/// spinner showed `prepare_tiles` costing 60-165ms per tick — several times the ~33ms the
/// spinner's own GIF frame needs — with the cost squarely in `render_card_tile`'s text
/// rasterization (cold `TextCache`/`FreeType`, expensive on this armv7 softfloat target,
/// not the windowed bookkeeping around it). One card per tick roughly halves the
/// worst-case stall at the cost of taking twice as many ticks to fill the window.
const CARD_BUILD_BUDGET: usize = 1;

/// How long the loading spinner waits for the whole window to become art-ready before
/// revealing anyway — a fetch that fails (rather than being merely slow) never becomes
/// ready, so this is what stops `grid_reveal_ready` from blocking forever.
const SPINNER_MAX_WAIT: Duration = Duration::from_millis(900);

/// How much the grid's focused card (and its ring) grows during
/// `ui::FOCUS_POP` — see `ui::zoom_rect`.
pub(crate) const CARD_GROWTH: f32 = 0.028;
/// How long a modal's fade/slide-in runs.
pub(crate) const MODAL_FADE: Duration = Duration::from_millis(170);
/// How long a scroll indicator (Settings' row list, About's document, ...) stays
/// at full opacity after a scroll before fading out.
pub(crate) const SCROLL_INDICATOR_HOLD: Duration = Duration::from_millis(700);
/// How long the indicator's fade-out takes once `SCROLL_INDICATOR_HOLD` elapses.
pub(crate) const SCROLL_INDICATOR_FADE: Duration = Duration::from_millis(350);
/// Total lifetime of one scroll indicator appearance — hold, then fade to gone.
pub(crate) const SCROLL_INDICATOR_LIFETIME: Duration =
    Duration::from_millis(SCROLL_INDICATOR_HOLD.as_millis() as u64 + SCROLL_INDICATOR_FADE.as_millis() as u64);
/// A few px wider than the indicator's track so its rounded caps aren't clipped.
const SCROLL_INDICATOR_TILE_W: u32 = 10;

/// Wrapped-visual-lines worth of the About document baked into one content
/// tile at a time (see `ui::ContentWindow`) — comfortably more than one
/// screenful in each direction so ordinary scrolling almost never triggers a
/// rebuild, while staying well under any plausible GLES2 max-texture-height
/// for this content's pixel width. Each rebuild renders every line in the
/// window uncached (`ui::draw_about_window` — see its docs on why), so this
/// is also a direct knob on how long one rebuild's hitch runs; kept smaller
/// than a "whole screen of slack" budget would otherwise be, so a rebuild
/// stays a brief blip rather than a rasterize-120-lines stall.
const ABOUT_WINDOW_BUDGET: usize = 80;
/// How close to the baked window's edge (in visual lines) the scroll position
/// must get before the window re-centers around it.
const ABOUT_WINDOW_MARGIN: usize = 16;

/// The pairing modal's subtitle — pulled out to a constant since both
/// `render_pairing` (drawing it) and `Self::pairing_pin_row_y` (measuring its
/// wrapped height, without drawing, to position the PIN row) need the exact
/// same string.
pub(crate) const PAIRING_SUBTITLE: &str = "Two ways to pair with this host — either one works.";

/// Shared card width for the four simple confirmation-style modals (Pairing,
/// `AddHost`, Wake, `ForgetHost`) — they used to each pick their own width
/// too (0.34-0.46 of screen width), which read as an inconsistent set of
/// window sizes popping up over Home. Height is deliberately *not* shared:
/// each of the four fits its own card to its own content (see
/// `pairing_card_rect`'s docs) rather than guessing one height that has to
/// fit all four (a fixed guess is exactly what clipped Wake's buttons once
/// its content changed). Settings is not one of these four at all: its row
/// list is real, variable content that needs the room, not a single
/// confirmation.
pub(crate) const SIMPLE_MODAL_WIDTH_FRAC: f32 = 0.40;

/// How often the magic packet is re-sent while a wake is in flight — see
/// `App::tick_wake`.
pub(crate) const WAKE_RESEND_INTERVAL: Duration = Duration::from_secs(15);

/// How long a *silent* auto-send (`Settings::wol_auto_send`) waits before giving up on
/// staying quiet and surfacing the wake prompt anyway — see `App::tick_wake`.
pub(crate) const WAKE_ESCALATE_AFTER: Duration = Duration::from_secs(60);

/// How often `App::tick_wake` actively re-checks reachability, independent of the WOL
/// timers above and of whether a MAC is even on record.
pub(crate) const WAKE_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// State for the "configured host is unreachable — wake it?" flow: both the interactive
/// prompt (`Screen::Wake`) and the silent background wait behind an auto-send live here,
/// distinguished by `silent`. Entered from `App::start_wake` whenever a known/paired
/// host's library fetch fails as genuinely unreachable, replacing the old plain grid
/// error message in every case — even with no MAC on record, where `render_wake` just
/// hides the send/auto-send controls instead.
pub struct WakeState {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) name: String,
    pub(crate) mac: Vec<String>,
    /// The original library error, restored into `home_status` if the user backs out
    /// without sending — so declining the prompt looks exactly like it did before this
    /// flow existed.
    pub(crate) reason: String,
    /// Focus on the modal: `0` = the "Always send automatically" toggle row,
    /// `1` = the "Wake" button, `2` = the "Cancel" button (see `render_wake`'s
    /// shell/buttons split, mirroring `Screen::ForgetHost`'s).
    pub(crate) focused: usize,
    /// Whether a packet has gone out for the current wait window.
    pub(crate) sent: bool,
    /// When the current wait window started (its first send) — drives the 60s
    /// escalation.
    pub(crate) since: Option<Instant>,
    pub(crate) last_attempt: Option<Instant>,
    /// `true` while this wait is running quietly because `wol_auto_send` fired it with
    /// no prompt shown — `App::tick_wake` flips it (and shows the prompt) once
    /// `WAKE_ESCALATE_AFTER` passes with the host still unreachable, so the user gets a
    /// chance to turn auto-send back off instead of it failing forever in silence.
    pub(crate) silent: bool,
    /// When the last active reachability probe went out — see `App::tick_wake`.
    pub(crate) last_probe: Option<Instant>,
    /// An in-flight reachability probe, if one is currently out.
    pub(crate) probe_rx: Option<std::sync::mpsc::Receiver<crate::library::GamesLoaded>>,
}

/// Which pane of Home currently has focus, and where within it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HomeFocus {
    /// Index into the sidebar: `0..entries.len()` are host rows,
    /// `entries.len()` is "+ Add host", `entries.len() + 1` is "Settings".
    Sidebar(usize),
    /// The ⋯ actions button on host row `usize` (always a host row — the utility rows
    /// have no actions button). Reached by pressing Right on the row; Right again
    /// continues to the grid, Left returns to the row. This replaced a hold-OK gesture:
    /// the actions existed but nothing on screen advertised them.
    SidebarMenu(usize),
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
pub(crate) struct PendingLaunch {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) fingerprint: [u8; 32],
    pub(crate) launch: Option<String>,
    pub(crate) rx: std::sync::mpsc::Receiver<crate::library::GamesLoaded>,
}

/// An open dropdown on the settings modal — `row` is the settings row index that
/// opened it (`ui::ROW_RESOLUTION`/`ui::ROW_FRAMERATE`/`ui::ROW_VIDEO_BACKEND`/
/// `ui::ROW_CODEC`/`ui::ROW_AUDIO`), `focused` is the highlighted option within
/// `ui::dropdown_options(settings, row)`.
pub struct DropdownState {
    pub row: usize,
    pub focused: usize,
}

/// What each modal's shell (every widget drawn unfocused, see `modal_tile`'s
/// docs) actually depends on — a value change invalidates it, but a pure
/// focus move must not, or the shell re-rasterizes (screen-sized) on every
/// single d-pad press regardless of the zoom-pop this whole mechanism exists
/// to make cheap. One enum since only one modal is ever open at a time.
/// Deliberately excludes whichever field only the *focused* widget's own
/// `ModalFocusKey` tracks (e.g. Settings excludes `settings_focused` itself,
/// the dropdown's focused option, Pairing excludes `pin_digit_index`) — moving
/// focus there is `modal_focus_tile`'s job, not this one's.
#[derive(PartialEq)]
pub(crate) enum ModalShellKey {
    Settings {
        settings: Settings,
        open_dropdown_row: Option<usize>,
        hover_close: bool,
    },
    Wake {
        name: String,
        mac_empty: bool,
        sent: bool,
        auto_send: bool,
        hover_close: bool,
    },
    Pairing {
        digits: [u8; 4],
        status: Option<String>,
        busy: bool,
        hover_close: bool,
    },
    ForgetHost {
        name: Option<String>,
        hover_close: bool,
    },
    /// The row list is derived purely from the host entry, so its identity plus the
    /// row count is enough to know the shell's pixels — the focused row is tracked
    /// separately by `ModalFocusKey::MenuRow`.
    HostMenu {
        name: String,
        subtitle: String,
        rows: usize,
        hover_close: bool,
    },
    /// The document itself never changes, and its scroll position no longer lives
    /// in this shell at all — the body is its own `Tile::ScrollContent(Screen::About)`
    /// tile, cropped/positioned by the GPU same as Settings' row list. So this shell
    /// is invalidated only by the same thing every other shell already excludes
    /// focus/scroll for: nothing screen-specific here at all.
    About {
        hover_close: bool,
    },
    SpeedTest {
        status: String,
        hover_close: bool,
    },
}

/// Identifies whichever single widget `App::modal_focus_tile` currently holds
/// — every modal screen (Settings/Wake/Pairing/ForgetHost) has exactly one
/// focused, zoom-animated widget at a time, and only one modal is ever open at
/// once, so one key type + one tile slot covers all of them instead of a
/// separate copy of this machinery per modal. Each variant carries whatever
/// that modal's focused widget's content actually depends on, so a value
/// change on it (not just a focus move) invalidates the tile too — e.g.
/// `SettingsRow` carries the whole `Settings` snapshot so dragging the
/// bitrate slider updates live (see the settings-focus-tile bug this fixed).
#[derive(PartialEq)]
pub(crate) enum ModalFocusKey {
    SettingsRow(usize, Settings),
    WakeToggle(bool),
    WakeButton(usize),
    PairingDigit(usize, u8),
    PairingButton,
    ForgetButton(usize),
    /// The speed test's focused button. Carries its label because the primary button's
    /// text is derived from the measurement — and because a bare index would compare
    /// equal to `ForgetButton`'s, letting a stale tile survive a screen change.
    SpeedTestButton(usize, String),
    /// Any `ListModal`-based screen's focused row. Carries the row's label as well as
    /// its index so a row list that changes shape under a fixed index (e.g. "Wake
    /// host" appearing once a MAC is learned) still invalidates the tile.
    MenuRow(usize, String),
}

/// What invalidates whichever modal's `Tile::ScrollContent(Screen)` is currently
/// baked (see `App::scroll_content_tile`) — one variant per screen that has
/// overflowing content. Paired with its `Screen` in the tile's staleness key, so
/// switching between two scrollable modals always rebuilds (there's only ever
/// one baked content tile, same "one modal open at a time" rationale as
/// `ModalFocusKey`).
#[derive(Clone, PartialEq)]
pub(crate) enum ScrollContentKey {
    /// Settings' row list — the whole `Settings` snapshot plus which row's
    /// dropdown (if any) is open, mirroring `ModalFocusKey::SettingsRow`.
    Settings(Settings, Option<usize>),
    /// About's document window — the wrapped-line index its content tile
    /// currently starts at (see `ui::ContentWindow`). The document itself
    /// never changes, so this is the only thing that invalidates it.
    About(usize),
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
    pub(crate) discovery_daemon: Option<mdns_sd::ServiceDaemon>,
    pub entries: Vec<HostEntry>,
    pub home_focus: HomeFocus,
    /// The sidebar host whose games are shown in the grid — `None` until one is
    /// selected (or restored from `store::load_selected_host` at startup).
    pub selected_host: Option<(String, u16)>,
    pub games: Vec<GameEntry>,
    /// Whether the current host has actually answered a library fetch — gates the
    /// fixed "Desktop" grid card (see `card_offset`), which has no reason to show
    /// before the host is confirmed reachable. Reset to `false` on every
    /// `select_host`; set once `drain_games`/`handle_library_error` hears back.
    pub(crate) games_loaded: bool,
    /// In-flight `library::fetch_games` call, if any — see `select_host`/`drain_games`.
    pub(crate) games_rx: Option<std::sync::mpsc::Receiver<crate::library::GamesLoaded>>,
    /// Library loading/error message shown in the grid area in place of cards.
    pub home_status: Option<String>,
    /// Decoded cover art, keyed by `GameEntry::id` — a `tiny_skia::Pixmap` composited
    /// straight into the frame `Painter`; see `art.rs` docs on why no separate
    /// GPU-texture-building step is needed here.
    pub art: std::collections::HashMap<String, Pixmap>,
    pub(crate) art_loader: Option<crate::art::ArtLoader>,
    /// The grid's current card size, refreshed every `prepare_tiles` call — read by
    /// `select_host`'s `ArtLoader::spawn` so covers arrive pre-stretched to fit.
    pub(crate) card_size: (u32, u32),
    pub(crate) pending_launch: Option<PendingLaunch>,
    /// Set by `drain_launch_check` on success — `main.rs` picks it up via
    /// `take_ready_launch` and starts the stream. Separate from `pending_launch`
    /// since that's cleared as soon as a result arrives, before `main.rs` can act.
    pub(crate) launch_ready: Option<ConnectTarget>,
    pub settings: Settings,
    /// Persists `settings` off the UI thread — see its docs on why a blocking
    /// `store::save_settings` on every adjustment read as input lag.
    pub(crate) settings_writer: store::SettingsWriter,
    pub settings_focused: usize,
    /// Offset/fade-in bookkeeping for whichever modal's content currently
    /// overflows its viewport (Settings' row list, About's document) — shared
    /// across screens since only one modal is ever open at a time, same
    /// rationale as `modal_focus_tile`. Reset whenever a scrollable modal is
    /// (re)opened (see `open_about`, `Screen::Settings`'s entry in `home.rs`).
    pub(crate) scroll: ui::ScrollWindow,
    /// Which slice of a too-long-for-one-texture document (About's) is
    /// currently baked into `scroll_content_tile` — see `ui::ContentWindow`.
    /// Settings never windows (its whole row list always fits one tile), so
    /// this is only ever meaningfully non-trivial for `Screen::About`.
    pub(crate) content_window: ui::ContentWindow,
    pub dropdown: Option<DropdownState>,
    /// The sidebar row `Screen::ForgetHost` is confirming forgetting — set
    /// alongside `screen = Screen::ForgetHost` (see `App::open_forget_host`),
    /// `None` otherwise.
    pub host_menu_index: Option<usize>,
    /// Which `Screen::ForgetHost` button has focus: `0` = "Forget", `1` =
    /// "Cancel". Defaults to Cancel (see `open_forget_host`) — a destructive
    /// action shouldn't be one more accidental OK press away.
    pub host_menu_focused: usize,
    /// Focused row of whichever `ListModal`-based screen is open (currently
    /// `Screen::HostMenu`). Separate from `host_menu_focused`, which is the
    /// Forget confirmation's two-button focus — the two screens can be open in
    /// sequence and must not share a cursor.
    pub menu_focused: usize,
    /// The sidebar row `Screen::EditHost` is editing, `None` otherwise.
    pub edit_host_index: Option<usize>,
    /// The in-flight/finished speed test, `None` when that screen isn't open.
    pub(crate) speed_test: Option<speedtest::SpeedTestState>,
    /// Delivers the background probe's progress/result — dropping it cancels.
    pub(crate) speed_test_rx: Option<std::sync::mpsc::Receiver<speedtest::SpeedTestMsg>>,
    /// Which of the finished test's two buttons has focus.
    pub speed_test_focused: usize,
    /// The host being measured, for the status line.
    pub speed_test_name: String,
    /// Last known reachability per `(host, port)` — see `app::reach`.
    pub(crate) reachable: std::collections::HashMap<(String, u16), bool>,
    pub(crate) reach_rx: Option<std::sync::mpsc::Receiver<reach::Reachability>>,
    pub(crate) reach_last: Option<Instant>,
    /// Whether webOS's on-screen keyboard is currently up, polled from
    /// `SDL_IsScreenKeyboardShown` each tick by `main.rs` — it moves the address form out
    /// from under the panel (see `App::keyboard_modal_card`).
    pub keyboard_shown: bool,
    /// The About document's source lines, built once on first open. ~10,000
    /// static string slices; cheap to hold, wasteful to rebuild per frame.
    pub about_lines: Vec<&'static str>,
    /// `about_lines` wrapped to a body width, flattened into one list of visual
    /// lines (see `ui::wrap_document`) — the unit `scroll`/`content_window`
    /// actually scroll over, since a source line's wrapped length varies and
    /// only the flattened list has a uniform per-unit stride. Keyed by the
    /// body width it was wrapped for, rebuilt if that width changes.
    pub(crate) about_wrapped: Option<(u32, Vec<String>)>,
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
    pub(crate) pairing_entry: usize,
    /// Whether the Magic Remote's pointer is currently hovering a modal's
    /// close (X) button.
    pub hover_close: bool,
    pub(crate) identity: (String, String),
    // ------------------------------------------------------------- GPU tiles --
    // Rasterized-once tile sources for the GPU compositor (`compositor.rs`):
    // `prepare_tiles` rebuilds whichever are stale and reports them for upload;
    // `draw_list` then composes each frame from their textures. Focus movement,
    // scrolling, and animations never re-rasterize anything.
    /// Focus-free sidebar strip (`SIDEBAR_W` × screen height): panel, brand
    /// mark + wordmark, every row unfocused. Stale when row content changes
    /// (`sidebar_dirty`), never on focus movement.
    pub(crate) sidebar_layer: Option<Painter>,
    pub(crate) sidebar_dirty: bool,
    /// Per-card tiles (shadow baked in, transparent padding), index-aligned
    /// with the grid. `None` = not yet rasterized (or invalidated).
    pub(crate) card_tiles: Vec<Option<Painter>>,
    /// All card tiles stale (games list / host changed).
    pub(crate) grid_dirty: bool,
    /// Card tiles still waiting to be rasterized inside the prefetch window. Keeps the
    /// main loop ticking until the window is filled — without it the redraw-on-change
    /// loop would go idle mid-build and leave blank cards on screen.
    pub(crate) tiles_pending: bool,
    /// Individual card tiles stale (cover art arrived) — cheaper than
    /// `grid_dirty` when the layout is unchanged.
    pub(crate) grid_cards_dirty: Vec<usize>,
    /// Tiles whose GPU texture should be released this frame — drained by `main.rs`,
    /// which owns the `Compositor`.
    pub(crate) evicted_tiles: Vec<Tile>,
    /// The shared focus-ring glow tile (one per card size).
    pub(crate) ring_tile: Option<Painter>,
    /// The focused sidebar row's tile, keyed by row index.
    pub(crate) focused_row_tile: Option<((usize, bool), Painter)>,
    /// The active modal rasterized full-screen (transparent surroundings);
    /// rebuilt on content changes, composited with fade/slide by the GPU. This
    /// is always the *shell* — every selectable widget drawn unfocused — with
    /// the actually focused one composited on top from `modal_focus_tile`
    /// instead of baked in here (see `ModalFocusKey`'s docs).
    pub(crate) modal_tile: Option<Painter>,
    /// What `modal_tile` was last rasterized from — a value change invalidates
    /// it, but moving focus alone must not (that's `modal_focus_tile`'s job).
    /// `None` while `Screen::Home`/`Screen::AddHost` (no `ModalShellKey`
    /// variant; `AddHost` just redraws on any `content_dirty` tick instead —
    /// its typed-digit display has no separate focus tile to protect).
    pub(crate) modal_shell_key: Option<ModalShellKey>,
    /// The single focused, zoom-animated widget of whichever modal is open —
    /// see `ModalFocusKey`'s docs on why one tile/key suffices for all of them.
    pub(crate) modal_focus_tile: Option<(ModalFocusKey, Painter)>,
    /// The open dropdown's panel + unfocused option list, keyed by its row (the
    /// options list depends only on which row opened it). Composited *after*
    /// `scroll_content_tile` — see `Tile::DropdownOverlay`'s docs.
    pub(crate) dropdown_overlay_tile: Option<(usize, Painter)>,
    /// The open dropdown's focused option, as its own small tile — keyed by
    /// (dropdown row, focused option index). Composited over `dropdown_overlay_tile`
    /// (which draws the overlay's option list unfocused); moving the
    /// dropdown's own focus rebuilds only this.
    pub(crate) dropdown_focus_tile: Option<((usize, usize), Painter)>,
    /// Whichever scrollable modal's indicator is baked, keyed by `(total units,
    /// visible units, scroll offset)` — rebuilt only when those change; the
    /// fade is a per-frame alpha, not baked into the tile. One slot for all of
    /// them (see `Tile::ScrollIndicator`'s docs — only one modal open at once).
    pub(crate) scroll_indicator_tile: Option<((usize, usize, usize), Painter)>,
    /// Whichever scrollable modal's content is baked, at full (unscrolled)
    /// height — keyed by `(Screen, ScrollContentKey)`. Scrolling within the
    /// baked window never invalidates this; see `Tile::ScrollContent`'s docs.
    pub(crate) scroll_content_tile: Option<((Screen, ScrollContentKey), Painter)>,
    /// Home's status line block, keyed by its text.
    pub(crate) status_tile: Option<(String, Painter)>,
    /// The static "No host selected" hint line.
    pub(crate) nohost_tile: Option<Painter>,
    /// Whether the grid's initial build for the current library has finished — while
    /// `false`, the grid shows the loading spinner (`Tile::SpinnerFrame`) instead of
    /// popping cards in one by one. One-shot per library: only `prepare_tiles`'s
    /// full-reset branch sets it `false` again; later scrolling into a fresh row
    /// does not.
    pub(crate) grid_reveal_ready: bool,
    /// The active spinner frame index shown while grid is loading.
    pub(crate) spinner_frame: Option<usize>,
    /// When the grid last became not-ready — feeds the spinner's rotation phase.
    pub(crate) spinner_since: Option<Instant>,
    // ------------------------------------------------------------ animations --
    /// Grid scroll offset actually rendered this frame (px; 0 = row 0 at
    /// `GRID_TOP_Y`) — eases toward `grid_scroll_target` each tick.
    pub grid_scroll: i32,
    pub(crate) grid_scroll_target: i32,
    /// When the current grid-focus pop started (card scales in over
    /// `ui::FOCUS_POP` — set on every d-pad focus move).
    pub(crate) focus_anim: Option<Instant>,
    /// When the open modal's fade/slide-in started (`MODAL_FADE`).
    pub(crate) modal_anim: Option<Instant>,
    /// When the open modal's focused widget last moved (zooms it in over
    /// `ui::FOCUS_POP`, same GPU-scale technique as `focus_anim` — see
    /// `draw_list`'s `Tile::ModalFocusElement` handling). Shared by every
    /// modal (Settings row, Wake row, Pairing digit/button, `ForgetHost`
    /// button) since only one is ever open, and focused, at a time.
    pub(crate) modal_focus_anim: Option<Instant>,
    /// In-flight `Toggle` row flip: `(when it started, the value it flipped
    /// from)` — lets `modal_focus_tile`'s render slide the switch knob from
    /// its old state to its new one over `ui::FOCUS_POP` instead of snapping.
    /// Shared by Settings' HDR/Stats-overlay toggles and Wake's auto-send one.
    pub(crate) switch_anim: Option<(Instant, bool)>,
    /// Last screen `prepare_tiles` saw — a change triggers the modal-open
    /// animation and a modal re-rasterize without every transition site
    /// needing to remember to.
    pub(crate) last_screen: Screen,
    /// In-flight PIN-pairing / request-access ceremony, delivering its outcome
    /// from a background thread — the ceremony blocks for up to minutes
    /// (request-access parks until a human approves it on the host), which used
    /// to freeze the whole UI when run inline on this thread. Drained by
    /// `drain_pairing` each tick; dropping the receiver (Back while busy)
    /// cancels: the worker's send fails and it exits.
    pub(crate) pairing_rx: Option<std::sync::mpsc::Receiver<PairingOutcome>>,
}

/// What a finished background pairing/request-access ceremony reports back —
/// everything needed to persist the host on success (captured going in, so the
/// worker doesn't need `App` access).
pub(crate) struct PairingOutcome {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) name: String,
    pub(crate) mgmt_port: Option<u16>,
    pub(crate) mac: Vec<String>,
    /// The host's now-verified fingerprint, or a user-displayable error.
    pub(crate) result: Result<[u8; 32], String>,
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(daemon) = &self.discovery_daemon {
            let _ = daemon.shutdown();
        }
    }
}

impl App {
    pub fn new(identity: (String, String)) -> Self {
        let known_hosts = store::load_known_hosts();
        let entries = known_hosts.iter().cloned().map(HostEntry::Known).collect();
        let (discovered, discovery_daemon) = match crate::discovery::browse() {
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
            games_loaded: false,
            games_rx: None,
            home_status: None,
            art: std::collections::HashMap::new(),
            art_loader: None,
            card_size: (0, 0),
            pending_launch: None,
            launch_ready: None,
            settings: store::load_settings(),
            settings_writer: store::SettingsWriter::spawn(),
            settings_focused: 0,
            scroll: ui::ScrollWindow::new(),
            content_window: ui::ContentWindow::new(),
            dropdown: None,
            host_menu_index: None,
            host_menu_focused: 1,
            menu_focused: 0,
            edit_host_index: None,
            speed_test: None,
            speed_test_rx: None,
            speed_test_focused: 0,
            speed_test_name: String::new(),
            reachable: Self::new_reachability(),
            reach_rx: None,
            reach_last: None,
            keyboard_shown: false,
            about_lines: Vec::new(),
            about_wrapped: None,
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
            tiles_pending: false,
            grid_cards_dirty: Vec::new(),
            evicted_tiles: Vec::new(),
            ring_tile: None,
            focused_row_tile: None,
            modal_tile: None,
            modal_shell_key: None,
            modal_focus_tile: None,
            dropdown_overlay_tile: None,
            dropdown_focus_tile: None,
            scroll_indicator_tile: None,
            scroll_content_tile: None,
            status_tile: None,
            nohost_tile: None,
            grid_reveal_ready: true,
            spinner_frame: None,
            spinner_since: None,
            grid_scroll: 0,
            grid_scroll_target: 0,
            focus_anim: None,
            modal_anim: None,
            modal_focus_anim: None,
            switch_anim: None,
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
                app.select_host(host, port, mgmt_port);
            }
        }
        // Decodes the spinner GIF now, off the render thread, so the LZW/frame-compose
        // cost lands here instead of stalling the first `draw_list` call that needs a
        // frame (right when the grid starts loading — the worst possible moment for a
        // render-thread hitch). `spinner_frames`'s `OnceLock` makes this a pure warm-up:
        // harmless if the spinner is drawn before this thread finishes, redundant work
        // (never a race) if it finishes first.
        std::thread::spawn(ui::spinner_frames);
        app
    }

    /// Merges freshly-discovered hosts into the entry list (known hosts keep their
    /// paired status; a discovered host not yet known gets appended), learns each
    /// known host's Wake-on-LAN MAC(s) from its live advert while it's awake to
    /// advertise them, and — if a wake is in flight (`self.wake`) — notices when the
    /// waking host reappears on mDNS and reconnects. Returns whether the sidebar
    /// actually changed — `main.rs`'s render loop uses this to skip a redraw when a
    /// discovery tick found nothing new (see its dirty-flag docs).
    pub fn drain_discovery(&mut self) -> bool {
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
            self.wake_succeeded(host, port, mgmt_port, "mDNS");
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
    pub(crate) fn wake_succeeded(&mut self, host: String, port: u16, mgmt_port: Option<u16>, source: &str) {
        tracing::info!("wake succeeded: {host}:{port} back ({source})");
        self.wake = None;
        self.screen = Screen::Home;
        self.select_host(host, port, mgmt_port);
    }

    /// Drains any cover art that's finished decoding since the last tick — called
    /// alongside `drain_discovery`. Returns whether any new art actually arrived
    /// (see `drain_discovery`'s docs on why).
    pub fn drain_art(&mut self) -> bool {
        let Some(loader) = &self.art_loader else { return false };
        let loaded = loader.drain();
        if loaded.is_empty() {
            return false;
        }
        for item in loaded {
            // Layout is unchanged by art arriving — queue a repaint of just that
            // card's tile (see `grid_cards_dirty`) rather than a full layer rebuild.
            if let Some(i) = self.games.iter().position(|g| g.id == item.game_id) {
                self.grid_cards_dirty.push(i + self.card_offset());
            }
            self.art.insert(item.game_id, item.pixmap);
        }
        true
    }
    /// Applies a `Back` to whichever screen is current — the single shared
    /// definition of "what Back means here" for every caller that needs it
    /// pre-emptively rather than through the normal per-screen `MenuEvent`
    /// dispatch: `main.rs`'s Back handling on Home (a no-op there, but routed
    /// through here so the policy lives in one place) and a modal's close (X)
    /// button click (`handle_mouse_click`'s `hover_close` branch below).
    pub fn back(&mut self) -> Option<ConnectTarget> {
        match self.screen {
            // Home has nothing to "back out" of (it's the root screen) — Back is a
            // no-op. (It used to be a shortcut straight to Settings, but that made
            // Back in Settings feel broken: close Settings, press Back again, and
            // Settings popped right back up.)
            Screen::Home => None,
            Screen::Pairing => {
                self.handle_pairing_event(MenuEvent::Back);
                None
            }
            Screen::Settings => {
                // `Back` never consults `screen_h` (only `Up`/`Down` scroll) — 0 is fine.
                self.handle_settings_event(MenuEvent::Back, 0);
                None
            }
            Screen::AddHost => {
                self.handle_add_host_event(MenuEvent::Back);
                None
            }
            Screen::Wake => {
                self.handle_wake_event(MenuEvent::Back);
                None
            }
            Screen::ForgetHost => {
                self.handle_forget_host_event(MenuEvent::Back);
                None
            }
            Screen::HostMenu => {
                self.handle_host_menu_event(MenuEvent::Back);
                None
            }
            Screen::SpeedTest => {
                self.handle_speed_test_event(MenuEvent::Back);
                None
            }
            Screen::EditHost => {
                self.handle_edit_host_event(MenuEvent::Back);
                None
            }
            // About's Back returns to Settings, not Home — see `handle_about_event`.
            // The screen size/fonts are irrelevant for a Back, so a zero probe is fine.
            Screen::About => {
                self.screen = Screen::Settings;
                None
            }
        }
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
            if t.elapsed() >= ui::FOCUS_POP {
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
        if let Some(t) = self.modal_focus_anim {
            if t.elapsed() >= ui::FOCUS_POP {
                self.modal_focus_anim = None;
            }
            animating = true;
        }
        if let Some((t, _)) = self.switch_anim {
            if t.elapsed() >= ui::FOCUS_POP {
                self.switch_anim = None;
            }
            animating = true;
        }
        if let Some(t) = self.scroll.shown_at {
            if t.elapsed() >= SCROLL_INDICATOR_LIFETIME {
                self.scroll.shown_at = None;
            }
            animating = true;
        }
        animating
    }
    // ---------------------------------------------------------------- mouse --

    /// Shared width, per-modal height: the four simple modals all size to
    /// `SIMPLE_MODAL_WIDTH_FRAC` but fit their own content (a shared *height*
    /// once clipped Wake's buttons — see the constant's docs). `content_height`
    /// receives a zero-y/height probe card at the final width and returns the
    /// card's total height.
    pub(crate) fn simple_modal_card(screen_w: u32, screen_h: u32, content_height: impl FnOnce(Rect) -> u32) -> Rect {
        let w = (screen_w as f32 * SIMPLE_MODAL_WIDTH_FRAC).round() as u32;
        let height = content_height(Rect::new(0, 0, w, 0));
        ui::modal_card_rect(screen_w, screen_h, SIMPLE_MODAL_WIDTH_FRAC, height)
    }

    /// Same, but for screens that raise the on-screen keyboard: the card sits where any
    /// other modal would until the panel actually appears, then lifts into the space above
    /// it (see `ui::modal_card_rect_above_keyboard`).
    ///
    /// Driven by `SDL_IsScreenKeyboardShown` rather than by "we asked for text input" —
    /// the panel can be dismissed while the field stays focused, and the card should drop
    /// back down when it is.
    pub(crate) fn keyboard_modal_card(
        &self,
        screen_w: u32,
        screen_h: u32,
        content_height: impl FnOnce(Rect) -> u32,
    ) -> Rect {
        let w = (screen_w as f32 * SIMPLE_MODAL_WIDTH_FRAC).round() as u32;
        let height = content_height(Rect::new(0, 0, w, 0));
        ui::modal_card_rect_above_keyboard(screen_w, screen_h, SIMPLE_MODAL_WIDTH_FRAC, height, self.keyboard_shown)
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
    pub fn handle_mouse_motion(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> bool {
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
            // above, which also tracks per-row hover) — same shape for all
            // four, just each its own card rect (see their docs on why that's
            // no longer a single shared size).
            Screen::Pairing => {
                let card = Self::pairing_card_rect(screen_w, screen_h, fonts);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            Screen::AddHost => {
                let card = self.address_card_rect(screen_w, screen_h, fonts);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            Screen::Wake => {
                let Some(wake) = &self.wake else { return false };
                let card = Self::wake_card_rect(screen_w, screen_h, wake, fonts);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            Screen::ForgetHost => {
                let name = self
                    .host_menu_index
                    .and_then(|i| self.entries.get(i))
                    .map(HostEntry::name)
                    .unwrap_or_default();
                let card = Self::forget_host_card_rect(screen_w, screen_h, name, fonts);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            Screen::HostMenu => {
                let subtitle = self.host_menu_subtitle();
                let rows = self.host_menu_actions().len();
                let card = Self::host_menu_card_rect(screen_w, screen_h, fonts, &subtitle, rows);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            Screen::EditHost => {
                let card = self.edit_host_card_rect(screen_w, screen_h, fonts);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            Screen::About => {
                let card = Self::about_card_rect(screen_w, screen_h);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
            Screen::SpeedTest => {
                let card = self.speed_test_card_rect(screen_w, screen_h, fonts);
                self.set_hover_close(ui::modal_close_rect(card).contains_point((x, y)))
            }
        }
    }

    /// Updates `hover_close` and reports whether it actually changed — every modal
    /// screen's close-button hover check in `handle_mouse_motion` follows this same
    /// shape.
    pub(crate) fn set_hover_close(&mut self, hover_close: bool) -> bool {
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
        fonts: &ui::Fonts,
    ) -> Option<ConnectTarget> {
        // Re-sync the close-button hover to the click's own position first — a
        // MouseButtonDown can carry a slightly different (x, y) than the last
        // MouseMotion (the physical button press can jostle the remote a little).
        self.handle_mouse_motion(x, y, screen_w, screen_h, fonts);
        if self.hover_close {
            // Same "what Back means here" as everywhere else — see `back`'s docs.
            return self.back();
        }
        // Unlike hover, a click DOES move `home_focus`/`settings_focused` — fresh at
        // the click's own position, so it confirms what was actually clicked rather
        // than whatever the keyboard/remote last focused elsewhere.
        match self.screen {
            Screen::Home => {
                // The ⋯ button sits inside its row, so it has to be tested first or the
                // click just reads as a click on the host.
                if let Some(idx) = ui::hit_test_sidebar_menu_button(x, y, self.entries.len()) {
                    self.home_focus = HomeFocus::SidebarMenu(idx);
                    self.open_host_menu(idx);
                    return None;
                }
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
                self.handle_home_event(MenuEvent::Confirm, screen_w, screen_h)
            }
            Screen::Settings => {
                // An open dropdown has no row grid of its own here — Confirm picks
                // whatever option `dd.focused` (moved by keyboard/remote only, same
                // as everywhere else) already points at; unaffected by this change.
                if self.dropdown.is_none() {
                    let (_, content) = Self::settings_layout(screen_w, screen_h);
                    let visible = Self::settings_visible_rows(screen_h);
                    // `local` is relative to the visible window; `?` bails if the click
                    // hit empty space within the card — nothing to focus or confirm.
                    let local = (0..visible).find(|&i| {
                        let row_y = content.y() + i as i32 * (ui::SETTINGS_ROW_H as i32 + ui::SETTINGS_ROW_GAP);
                        Rect::new(content.x(), row_y, content.width(), ui::SETTINGS_ROW_H).contains_point((x, y))
                    })?;
                    self.settings_focused = self.scroll.clamped(ui::SETTINGS_ROW_COUNT, visible) + local;
                }
                self.handle_settings_event(MenuEvent::Confirm, screen_h);
                None
            }
            Screen::Pairing => {
                // The Magic Remote pointer is the most reliable input on this TV, so the
                // "Request access" button is clickable directly: focus it and confirm.
                let card = Self::pairing_card_rect(screen_w, screen_h, fonts);
                if Self::pairing_request_button_rect(card, fonts).contains_point((x, y)) {
                    self.pairing_focus = PairingFocus::RequestAccess;
                    self.handle_pairing_event(MenuEvent::Confirm);
                }
                None
            }
            Screen::Wake => {
                self.handle_wake_event(MenuEvent::Confirm);
                None
            }
            Screen::ForgetHost => {
                self.handle_forget_host_event(MenuEvent::Confirm);
                None
            }
            // A click focuses the row it landed on first, then confirms it — same
            // click-moves-focus rule as Home/Settings above.
            Screen::HostMenu => {
                let subtitle = self.host_menu_subtitle();
                let rows = self.host_menu_actions().len();
                let card = Self::host_menu_card_rect(screen_w, screen_h, fonts, &subtitle, rows);
                let content = ui::list_modal_content_rect(card, fonts, &subtitle, rows);
                let i = (0..rows).find(|&i| ui::focus_row_rect(content, i).contains_point((x, y)))?;
                self.menu_focused = i;
                self.handle_host_menu_event(MenuEvent::Confirm);
                None
            }
            Screen::SpeedTest => {
                self.handle_speed_test_event(MenuEvent::Confirm);
                None
            }
            // Nothing clickable but the close button (handled above).
            Screen::AddHost | Screen::EditHost | Screen::About => None,
        }
    }
    // --------------------------------------------------------------- render --

    /// `1` when the fixed "Desktop" card occupies grid index `0` — only once
    /// `games_loaded`, i.e. the host has actually answered a library fetch — `0`
    /// otherwise, so index `0` is the first game directly. Subtract this from a
    /// grid index before indexing into `games`.
    pub(crate) fn card_offset(&self) -> usize {
        usize::from(self.games_loaded)
    }

    /// The title of grid card `idx` (0 = the fixed "Desktop" card, when shown — see
    /// `card_offset`) and its cover art, if fetched.
    pub(crate) fn grid_card_content(&self, idx: usize) -> (&str, Option<&Pixmap>) {
        if self.games_loaded && idx == 0 {
            ("Desktop", None)
        } else {
            let game = &self.games[idx - self.card_offset()];
            (game.title.as_str(), self.art.get(&game.id))
        }
    }

    /// The current position (0.0..=1.0, see `ui::draw_switch`) of a `Toggle`
    /// row's switch given its settled state `target_on` — mid-slide while
    /// `switch_anim` is in flight *for that same transition*, otherwise
    /// settled at the endpoint.
    pub(crate) fn toggle_frac(&self, target_on: bool) -> f32 {
        match self.switch_anim {
            Some((t, from_on)) if from_on != target_on => {
                let f = ui::anim_frac(Some(t), ui::FOCUS_POP);
                if target_on {
                    f
                } else {
                    1.0 - f
                }
            }
            _ => f32::from(target_on),
        }
    }

    /// Rasterizes every stale tile (tiny-skia, CPU — the only place rasterization
    /// happens) and returns which tiles need their GPU texture re-uploaded.
    /// `content_dirty` is the main loop's "an event/drain changed something this
    /// tick" flag — it forces the open modal's tile to re-rasterize, since modal
    /// content has no finer dirty tracking of its own. Pure animation frames pass
    /// `false` and rasterize nothing at all.
    pub fn prepare_tiles(
        &mut self,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
        content_dirty: bool,
    ) -> Result<Vec<Tile>> {
        let mut updated = Vec::new();
        let available_w = screen_w.saturating_sub(ui::SIDEBAR_W);
        let columns = ui::grid_columns(available_w);
        let (card_w, card_h) = ui::grid_card_size(available_w, columns);
        self.card_size = (card_w, card_h);

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
                fonts,
                &self.entries,
                None,
                &self.reachability_list(),
                screen_h,
            )?;
            self.sidebar_layer = Some(layer);
            self.sidebar_dirty = false;
            self.focused_row_tile = None; // row content may have changed under it
            updated.push(Tile::Sidebar);
        }
        // One tile serves both sidebar focus states (see `render_focused_row_tile`).
        let sidebar_focus = match self.home_focus {
            HomeFocus::Sidebar(i) => Some((i, false)),
            HomeFocus::SidebarMenu(i) => Some((i, true)),
            HomeFocus::Grid(_) => None,
        };
        if let Some(key) = sidebar_focus {
            let stale = !matches!(&self.focused_row_tile, Some((k, _)) if *k == key);
            if stale {
                let online = self.entries.get(key.0).and_then(|e| self.entry_online(e));
                let tile = ui::render_focused_row_tile(text_cache, fonts, &self.entries, key.0, key.1, online)?;
                self.focused_row_tile = Some((key, tile));
                updated.push(Tile::FocusRow);
            }
        }

        // Reset before the branch: it is only ever set inside it, and a stale `true` left
        // behind by a host that has since been deselected would spin the render loop at
        // full rate forever.
        self.tiles_pending = false;
        if self.selected_host.is_some() {
            let count = self.grid_len();
            if self.grid_dirty || self.card_tiles.len() != count {
                // Every existing texture is stale (different library, or a different
                // layout) — drop them rather than leaving them to be overwritten one by
                // one, which would strand the tail of a longer previous library.
                for idx in 0..self.card_tiles.len() {
                    self.evicted_tiles.push(Tile::Card(idx));
                }
                self.card_tiles = std::iter::repeat_with(|| None).take(count).collect();
                self.grid_dirty = false;
                self.grid_cards_dirty.clear();
                // Only a full reset re-arms the spinner — ordinary scrolling into the
                // prefetch window must not hide the grid again.
                self.grid_reveal_ready = false;
                self.spinner_since = None;
                self.spinner_frame = None;
            } else {
                for idx in std::mem::take(&mut self.grid_cards_dirty) {
                    if idx < count {
                        self.card_tiles[idx] = None;
                    }
                }
            }

            // Windowed, budgeted tile building — see `CARD_BUILD_BUDGET`.
            let row_h = card_h as i32 + ui::GRID_GAP;
            let visible_rows = (screen_h as i32 - ui::GRID_TOP_Y).max(row_h) / row_h + 1;
            let first_visible_row = (self.grid_scroll / row_h).max(0);
            let row_of = |idx: usize| (idx / columns.max(1)) as i32;
            let build_lo = first_visible_row - CARD_PREFETCH_ROWS;
            let build_hi = first_visible_row + visible_rows + CARD_PREFETCH_ROWS;
            let keep_lo = first_visible_row - CARD_KEEP_ROWS;
            let keep_hi = first_visible_row + visible_rows + CARD_KEEP_ROWS;

            // Evict first, so a long scroll frees textures in the same frame it needs new
            // ones rather than a frame later.
            for idx in 0..count {
                let row = row_of(idx);
                if (row < keep_lo || row > keep_hi) && self.card_tiles[idx].is_some() {
                    self.card_tiles[idx] = None;
                    self.evicted_tiles.push(Tile::Card(idx));
                    if let Some(game) = self.games.get(idx.wrapping_sub(self.card_offset())) {
                        // Drop the decoded cover too — it is several times the size of the
                        // tile it feeds. Re-requested from the disk cache on scroll back.
                        self.art.remove(&game.id);
                        if let Some(loader) = &mut self.art_loader {
                            loader.forget(&game.id);
                        }
                    }
                }
            }

            // Ready once nothing more can arrive: cover already in `self.art`, or the game
            // never had one to fetch (no `self.art` entry either way). `idx == 0` (Desktop,
            // when shown) has no `games` entry and is always ready.
            let card_offset = self.card_offset();
            let art_ready = |idx: usize| {
                self.games.get(idx.wrapping_sub(card_offset)).is_none_or(|game| {
                    self.art.contains_key(&game.id) || (game.art.portrait.is_none() && game.art.header.is_none())
                })
            };

            // Art-ready cards build first — building one before its cover arrives just
            // burns a second budget slot re-dirtying it once the cover shows up.
            let mut to_build = Vec::new();
            for idx in 0..count {
                let row = row_of(idx);
                if row < build_lo || row > build_hi {
                    continue;
                }
                // Ask for this card's cover as it enters the window, not for the whole
                // library at once (see `art::ArtLoader`).
                if let (Some(loader), Some(game)) =
                    (&mut self.art_loader, self.games.get(idx.wrapping_sub(card_offset)))
                {
                    loader.request(game);
                }
                if self.card_tiles[idx].is_some() {
                    continue;
                }
                to_build.push((idx, art_ready(idx)));
            }
            to_build.sort_by_key(|(_, ready)| !ready);

            let mut pending = false;
            for (built, (idx, _)) in to_build.into_iter().enumerate() {
                if built >= CARD_BUILD_BUDGET {
                    pending = true;
                    break;
                }
                let tile = {
                    let (title, art) = self.grid_card_content(idx);
                    ui::render_card_tile(text_cache, fonts, card_w, card_h, title, art)?
                };
                self.card_tiles[idx] = Some(tile);
                updated.push(Tile::Card(idx));
            }
            self.tiles_pending = pending;

            // Rechecks the whole window rather than trusting `!pending`, since a card
            // built earlier can still be waiting behind a re-dirtied sibling; requires
            // `art_ready` too so a placeholder built this tick can't count as revealed.
            if !self.grid_reveal_ready {
                let window_ready = (0..count)
                    .filter(|&idx| {
                        let row = row_of(idx);
                        row >= build_lo && row <= build_hi
                    })
                    .all(|idx| self.card_tiles[idx].is_some() && art_ready(idx));
                let since = *self.spinner_since.get_or_insert_with(Instant::now);
                self.grid_reveal_ready = window_ready || since.elapsed() >= SPINNER_MAX_WAIT;
                if self.grid_reveal_ready {
                    self.spinner_since = None;
                    self.spinner_frame = None;
                } else {
                    let (frame_idx, _) = ui::spinner_frame_at(since.elapsed().as_secs_f32());
                    if self.spinner_frame != Some(frame_idx) {
                        self.spinner_frame = Some(frame_idx);
                        updated.push(Tile::SpinnerFrame(frame_idx));
                    }
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
                        let tile = ui::render_wrapped_text_tile(text_cache, fonts.label, s, max_w, ui::MUTED, 6)?;
                        self.status_tile = Some((s.clone(), tile));
                        updated.push(Tile::Status);
                    }
                }
                None => self.status_tile = None,
            }
        } else {
            self.grid_reveal_ready = true;
            self.spinner_since = None;
            if self.nohost_tile.is_none() {
                self.nohost_tile = Some(ui::render_text_tile(
                    text_cache,
                    fonts.label,
                    "No host selected — pick one from the list, or add one.",
                    ui::MUTED,
                )?);
                updated.push(Tile::NoHost);
            }
        }

        let modal_open = !matches!(self.screen, Screen::Home);
        // Every modal's shell only reacts to *content* changes — not to
        // `content_dirty`, which is also `true` on plain focus movement (see
        // `ModalShellKey`'s docs). `AddHost` has no `ModalShellKey` variant
        // (no split focus tile to protect) and just redraws on any
        // `content_dirty` tick, same as every modal did before this split.
        let modal_shell_key = match self.screen {
            Screen::Settings => Some(ModalShellKey::Settings {
                settings: self.settings,
                open_dropdown_row: self.dropdown.as_ref().map(|dd| dd.row),
                hover_close: self.hover_close,
            }),
            Screen::Wake => self.wake.as_ref().map(|w| ModalShellKey::Wake {
                name: w.name.clone(),
                mac_empty: w.mac.is_empty(),
                sent: w.sent,
                auto_send: self.settings.wol_auto_send,
                hover_close: self.hover_close,
            }),
            Screen::Pairing => Some(ModalShellKey::Pairing {
                digits: self.pin_digits,
                status: self.pairing_status.clone(),
                busy: self.pairing_busy,
                hover_close: self.hover_close,
            }),
            Screen::ForgetHost => Some(ModalShellKey::ForgetHost {
                name: self
                    .host_menu_index
                    .and_then(|i| self.entries.get(i))
                    .map(|e| e.name().to_string()),
                hover_close: self.hover_close,
            }),
            Screen::HostMenu => Some(ModalShellKey::HostMenu {
                name: self.host_menu_title(),
                subtitle: self.host_menu_subtitle(),
                rows: self.host_menu_actions().len(),
                hover_close: self.hover_close,
            }),
            Screen::About => Some(ModalShellKey::About {
                hover_close: self.hover_close,
            }),
            // The whole shell is derived from the status sentence, which already encodes
            // the phase and the latest measurement.
            Screen::SpeedTest => Some(ModalShellKey::SpeedTest {
                status: self.speed_test_status(),
                hover_close: self.hover_close,
            }),
            // `EditHost` joins `AddHost` in having no shell key: its typed-digit
            // display has no separate focus tile to protect, so it just redraws on
            // any `content_dirty` tick.
            Screen::Home | Screen::AddHost | Screen::EditHost => None,
        };
        let modal_stale = if modal_shell_key.is_some() {
            self.modal_tile.is_none() || self.modal_shell_key != modal_shell_key
        } else {
            content_dirty || self.modal_tile.is_none()
        };
        self.modal_shell_key = modal_shell_key;
        if modal_open && (screen_changed || modal_stale) {
            let mut p = match self.modal_tile.take() {
                Some(p) => p,
                None => Painter::new(screen_w, screen_h),
            };
            p.clear_transparent();
            match self.screen {
                Screen::Home => unreachable!("modal_open checked above"),
                Screen::Pairing => {
                    self.render_pairing(&mut p, text_cache, fonts, screen_w, screen_h)?;
                }
                Screen::Settings => {
                    self.render_settings(&mut p, text_cache, fonts, screen_w, screen_h)?;
                }
                Screen::AddHost => self.render_add_host(&mut p, text_cache, fonts, screen_w, screen_h)?,
                Screen::Wake => {
                    self.render_wake(&mut p, text_cache, fonts, screen_w, screen_h)?;
                }
                Screen::ForgetHost => {
                    self.render_forget_host(&mut p, text_cache, fonts, screen_w, screen_h)?;
                }
                Screen::HostMenu => {
                    self.render_host_menu(&mut p, text_cache, fonts, screen_w, screen_h)?;
                }
                Screen::EditHost => self.render_edit_host(&mut p, text_cache, fonts, screen_w, screen_h)?,
                Screen::About => self.render_about(&mut p, text_cache, fonts, screen_w, screen_h)?,
                Screen::SpeedTest => self.render_speed_test(&mut p, text_cache, fonts, screen_w, screen_h)?,
            }
            self.modal_tile = Some(p);
            updated.push(Tile::Modal);
        }
        // Whichever modal is open has at most one focused, zoom-animated widget
        // (`ModalFocusKey`'s docs) — `None` for screens with no such widget
        // (Home, AddHost) or when Wake has nothing to focus (no MAC on record,
        // see `handle_wake_event`'s matching guard).
        let focus_key = match self.screen {
            Screen::Settings => Some(ModalFocusKey::SettingsRow(self.settings_focused, self.settings)),
            Screen::Wake => self.wake.as_ref().filter(|w| !w.mac.is_empty()).map(|w| {
                if w.focused == 0 {
                    ModalFocusKey::WakeToggle(self.settings.wol_auto_send)
                } else {
                    ModalFocusKey::WakeButton(w.focused)
                }
            }),
            Screen::Pairing => Some(match self.pairing_focus {
                PairingFocus::Pin => {
                    ModalFocusKey::PairingDigit(self.pin_digit_index, self.pin_digits[self.pin_digit_index])
                }
                PairingFocus::RequestAccess => ModalFocusKey::PairingButton,
            }),
            Screen::ForgetHost => Some(ModalFocusKey::ForgetButton(self.host_menu_focused)),
            Screen::HostMenu => self
                .host_menu_actions()
                .get(self.menu_focused)
                .map(|(_, row)| ModalFocusKey::MenuRow(self.menu_focused, row.label.clone())),
            // Only once there are buttons to focus — while measuring there is nothing
            // on the card but text.
            Screen::SpeedTest => matches!(
                self.speed_test,
                Some(speedtest::SpeedTestState::Done { .. }) | Some(speedtest::SpeedTestState::Failed(_))
            )
            .then(|| {
                let recommended = match &self.speed_test {
                    Some(speedtest::SpeedTestState::Done { outcome, .. }) => Self::recommended_kbps(outcome),
                    _ => None,
                };
                ModalFocusKey::SpeedTestButton(self.speed_test_focused, Self::speed_test_apply_label(recommended))
            }),
            // Neither has a single focused widget: the address form is one always-active
            // field, and About is a scrolling document.
            Screen::Home | Screen::AddHost | Screen::EditHost | Screen::About => None,
        };
        if let Some(key) = focus_key {
            // Also stale on every tick of an in-flight `switch_anim`: the knob's
            // position depends on elapsed time, not on `key`, which doesn't
            // change mid-flip.
            let stale = self.switch_anim.is_some() || !matches!(&self.modal_focus_tile, Some((k, _)) if *k == key);
            if stale {
                let tile = match self.screen {
                    Screen::Settings => {
                        let (_, content) = Self::settings_layout(screen_w, screen_h);
                        let rows = ui::settings_rows(&self.settings);
                        let dropdown_open = self.dropdown.as_ref().is_some_and(|dd| dd.row == self.settings_focused);
                        let target_on = rows.get(self.settings_focused).is_some_and(|r| r.value == "On");
                        ui::render_focus_row_tile(
                            text_cache,
                            fonts,
                            &rows,
                            content.width(),
                            self.settings_focused,
                            dropdown_open,
                            self.toggle_frac(target_on),
                        )?
                    }
                    Screen::Wake => {
                        let wake = self
                            .wake
                            .as_ref()
                            .expect("focus_key only Some for a Wake with a focusable widget");
                        let card = Self::wake_card_rect(screen_w, screen_h, wake, fonts);
                        if wake.focused == 0 {
                            let rows = ui::wake_rows(self.settings.wol_auto_send);
                            let content_w = Self::wake_toggle_rect(card, wake, fonts).width();
                            ui::render_focus_row_tile(
                                text_cache,
                                fonts,
                                &rows,
                                content_w,
                                0,
                                false,
                                self.toggle_frac(self.settings.wol_auto_send),
                            )?
                        } else {
                            let toggle = Self::wake_toggle_rect(card, wake, fonts);
                            let rect = ui::confirm_button_rect(Self::wake_buttons_rect(toggle), wake.focused - 1);
                            let buttons = Self::wake_buttons();
                            ui::render_confirm_button_tile(
                                text_cache,
                                fonts,
                                &buttons[wake.focused - 1],
                                rect.width(),
                                rect.height(),
                            )?
                        }
                    }
                    Screen::Pairing => match self.pairing_focus {
                        PairingFocus::Pin => ui::render_pairing_digit_tile(
                            text_cache,
                            fonts.title,
                            self.pin_digits[self.pin_digit_index],
                        )?,
                        PairingFocus::RequestAccess => {
                            let card = Self::pairing_card_rect(screen_w, screen_h, fonts);
                            let btn = Self::pairing_request_button_rect(card, fonts);
                            ui::render_pairing_button_tile(text_cache, fonts.label, btn.width(), btn.height())?
                        }
                    },
                    Screen::ForgetHost => {
                        let name = self
                            .host_menu_index
                            .and_then(|i| self.entries.get(i))
                            .map(HostEntry::name)
                            .unwrap_or_default();
                        let card = Self::forget_host_card_rect(screen_w, screen_h, name, fonts);
                        let content = Self::forget_host_content_rect(card, name, fonts);
                        let rect = ui::confirm_button_rect(content, self.host_menu_focused);
                        let buttons = Self::forget_buttons();
                        ui::render_confirm_button_tile(
                            text_cache,
                            fonts,
                            &buttons[self.host_menu_focused],
                            rect.width(),
                            rect.height(),
                        )?
                    }
                    Screen::HostMenu => {
                        let subtitle = self.host_menu_subtitle();
                        let rows = self.host_menu_rows();
                        let card = Self::host_menu_card_rect(screen_w, screen_h, fonts, &subtitle, rows.len());
                        let content = ui::list_modal_content_rect(card, fonts, &subtitle, rows.len());
                        ui::render_focus_row_tile(
                            text_cache,
                            fonts,
                            &rows,
                            content.width(),
                            self.menu_focused,
                            false,
                            0.0,
                        )?
                    }
                    Screen::SpeedTest => {
                        let card = self.speed_test_card_rect(screen_w, screen_h, fonts);
                        let rect =
                            ui::confirm_button_rect(self.speed_test_buttons_rect(card, fonts), self.speed_test_focused);
                        let recommended = match &self.speed_test {
                            Some(speedtest::SpeedTestState::Done { outcome, .. }) => Self::recommended_kbps(outcome),
                            _ => None,
                        };
                        let apply_label = Self::speed_test_apply_label(recommended);
                        let buttons = Self::speed_test_buttons(&apply_label);
                        ui::render_confirm_button_tile(
                            text_cache,
                            fonts,
                            &buttons[self.speed_test_focused],
                            rect.width(),
                            rect.height(),
                        )?
                    }
                    Screen::Home | Screen::AddHost | Screen::EditHost | Screen::About => {
                        unreachable!("focus_key checked above")
                    }
                };
                self.modal_focus_tile = Some((key, tile));
                updated.push(Tile::ModalFocusElement);
            }
        } else {
            self.modal_focus_tile = None;
        }

        if let Some(dd) = &self.dropdown {
            let (_, content) = Self::settings_layout(screen_w, screen_h);
            let options = ui::dropdown_options(&self.settings, dd.row);

            let overlay_stale = !matches!(&self.dropdown_overlay_tile, Some((k, _)) if *k == dd.row);
            if overlay_stale {
                let overlay_h = options.len() as u32 * ui::DROPDOWN_OPTION_H;
                let mut p = Painter::new(content.width(), overlay_h.max(1));
                let rect = Rect::new(0, 0, content.width(), overlay_h);
                ui::draw_dropdown_overlay(&mut p, text_cache, fonts.value, &options, usize::MAX, rect)?;
                self.dropdown_overlay_tile = Some((dd.row, p));
                updated.push(Tile::DropdownOverlay);
            }

            let key = (dd.row, dd.focused);
            let stale = !matches!(&self.dropdown_focus_tile, Some((k, _)) if *k == key);
            if stale {
                let option = options.get(dd.focused).map_or("", String::as_str);
                let tile = ui::render_dropdown_option_tile(text_cache, fonts.value, option, content.width())?;
                self.dropdown_focus_tile = Some((key, tile));
                updated.push(Tile::DropdownFocusOption);
            }
        } else {
            self.dropdown_overlay_tile = None;
            self.dropdown_focus_tile = None;
        }

        // Whichever modal's content overflows its viewport (Settings' rows, About's
        // document) gets its scroll indicator and content tile refreshed here — see
        // `scroll_geometry`'s docs for why this one block covers every such modal
        // instead of being hand-copied per screen.
        if matches!(self.screen, Screen::About) {
            // Mutates `about_wrapped` only — must happen before `scroll_geometry`
            // (a `&self` read) can report a non-zero total for this frame.
            let card = ui::about_card_rect(screen_w, screen_h);
            let body = ui::about_body_rect(card, fonts);
            self.ensure_about_wrapped(fonts, body.width());
        }
        if let Some((total, visible, _, content)) = self.scroll_geometry(screen_w, screen_h, fonts) {
            let scroll = self.scroll.clamped(total, visible);
            let ind_key = (total, visible, scroll);
            let ind_stale = !matches!(&self.scroll_indicator_tile, Some((k, _)) if *k == ind_key);
            if ind_stale {
                let tile =
                    ui::render_list_scrollbar_tile(SCROLL_INDICATOR_TILE_W, content.height(), total, visible, scroll);
                self.scroll_indicator_tile = Some((ind_key, tile));
                updated.push(Tile::ScrollIndicator(self.screen));
            }

            match self.screen {
                Screen::Settings => {
                    let dropdown_row = self.dropdown.as_ref().map(|dd| dd.row);
                    let key = (
                        Screen::Settings,
                        ScrollContentKey::Settings(self.settings, dropdown_row),
                    );
                    let stale = !matches!(&self.scroll_content_tile, Some((k, _)) if *k == key);
                    if stale {
                        let rows = ui::settings_rows(&self.settings);
                        let tile = ui::render_focus_rows_tile(text_cache, fonts, &rows, content.width(), dropdown_row)?;
                        self.scroll_content_tile = Some((key, tile));
                        // Settings' whole row list always fits one tile — no windowing.
                        self.content_window = ui::ContentWindow {
                            start: 0,
                            len: ui::SETTINGS_ROW_COUNT,
                        };
                        updated.push(Tile::ScrollContent(Screen::Settings));
                    }
                }
                Screen::About => {
                    if let Some(new_start) = self.content_window.recenter_if_needed(
                        scroll,
                        visible,
                        total,
                        ABOUT_WINDOW_BUDGET,
                        ABOUT_WINDOW_MARGIN,
                    ) {
                        let len = ABOUT_WINDOW_BUDGET.min(total.saturating_sub(new_start));
                        if let Some((_, wrapped)) = &self.about_wrapped {
                            let stride = self.scroll_stride(fonts) as u32;
                            let mut p = Painter::new(content.width().max(1), (len as u32 * stride).max(1));
                            ui::draw_about_window(&mut p, fonts.value, wrapped, new_start, len)?;
                            self.content_window = ui::ContentWindow { start: new_start, len };
                            self.scroll_content_tile = Some(((Screen::About, ScrollContentKey::About(new_start)), p));
                            updated.push(Tile::ScrollContent(Screen::About));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(updated)
    }

    /// `(total units, visible units, card rect, content/viewport rect)` for whichever
    /// scrollable modal is open — `None` if `self.screen` has no overflowing content.
    /// The one place this per-modal geometry lives, shared by `prepare_tiles`'s
    /// staleness checks and `draw_list`'s GPU-crop math so the two can't disagree.
    /// `About`'s `total` depends on `about_wrapped` already being fresh for this
    /// frame's body width — `prepare_tiles` ensures that before calling this;
    /// `draw_list` runs after `prepare_tiles` in the same frame, so it's already set.
    pub(crate) fn scroll_geometry(
        &self,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::Fonts,
    ) -> Option<(usize, usize, Rect, Rect)> {
        match self.screen {
            Screen::Settings => {
                let (card, content) = Self::settings_layout(screen_w, screen_h);
                let visible = Self::settings_visible_rows(screen_h);
                Some((ui::SETTINGS_ROW_COUNT, visible, card, content))
            }
            Screen::About => {
                let card = ui::about_card_rect(screen_w, screen_h);
                let body = ui::about_body_rect(card, fonts);
                let total = self.about_wrapped.as_ref().map_or(0, |(_, v)| v.len());
                let visible = ui::about_visible_lines(body, fonts.value);
                Some((total, visible, card, body))
            }
            _ => None,
        }
    }

    /// Pixel stride between two consecutive units of whichever modal is scrolling —
    /// Settings' fixed row height, or About's wrapped-line height. Only meaningful
    /// when `scroll_geometry` returns `Some`.
    fn scroll_stride(&self, fonts: &ui::Fonts) -> i32 {
        match self.screen {
            Screen::Settings => ui::SETTINGS_ROW_H as i32 + ui::SETTINGS_ROW_GAP,
            Screen::About => ui::about_line_stride(fonts.value),
            _ => 1,
        }
    }

    /// The pixmap behind `tile`, for the compositor to upload.
    pub fn tile_pixmap(&self, tile: Tile) -> Option<&Painter> {
        match tile {
            Tile::Sidebar => self.sidebar_layer.as_ref(),
            Tile::FocusRow => self.focused_row_tile.as_ref().map(|(_, p)| p),
            Tile::Card(i) => self.card_tiles.get(i).and_then(|t| t.as_ref()),
            Tile::Ring => self.ring_tile.as_ref(),
            Tile::Modal => self.modal_tile.as_ref(),
            Tile::ModalFocusElement => self.modal_focus_tile.as_ref().map(|(_, p)| p),
            Tile::DropdownOverlay => self.dropdown_overlay_tile.as_ref().map(|(_, p)| p),
            Tile::DropdownFocusOption => self.dropdown_focus_tile.as_ref().map(|(_, p)| p),
            Tile::ScrollIndicator(_) => self.scroll_indicator_tile.as_ref().map(|(_, p)| p),
            Tile::ScrollContent(_) => self.scroll_content_tile.as_ref().map(|(_, p)| p),
            Tile::Status => self.status_tile.as_ref().map(|(_, p)| p),
            Tile::NoHost => self.nohost_tile.as_ref(),
            // `SpinnerFrame` is uploaded directly from its raw decoded pixels (see
            // `main.rs`), never rasterized as a `Painter`; the rest are stream-side only
            // (uploaded directly by `run_inner`'s overlay refresh) — never one of App's
            // menu tiles.
            Tile::SpinnerFrame(_) | Tile::StatsOverlay | Tile::DisconnectDialog | Tile::DisconnectFocusButton => {
                None
            }
        }
    }

    /// Builds this frame's draw list (paint order) from the current state and
    /// animation clocks — pure bookkeeping, no rasterization (the font
    /// params are only for pure geometry — `ui::modal_header_end_y` and
    /// friends — needed to position a modal's focused-widget tile without
    /// re-rendering its header). The GPU executes it (`Compositor::execute`).
    pub fn draw_list(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Vec<DrawCmd> {
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
        } else if !self.grid_reveal_ready {
            let phase = self.spinner_since.map_or(0.0, |s| s.elapsed().as_secs_f32());
            let (idx, frame) = ui::spinner_frame_at(phase);
            let x = grid_x + (available_w as i32 - frame.width as i32) / 2;
            // 40% down rather than dead-center, which reads as slightly low on a TV.
            let area_h = screen_h as i32 - ui::GRID_TOP_Y;
            let y = ui::GRID_TOP_Y + (area_h - frame.height as i32) * 2 / 5;
            cmds.push(DrawCmd::Tex {
                tile: Tile::SpinnerFrame(idx),
                dst: Rect::new(x, y, frame.width, frame.height),
                alpha: 0xff,
            });
        } else {
            let count = self.grid_len();
            let focused = match self.home_focus {
                HomeFocus::Grid(i) if i < count => Some(i),
                HomeFocus::Grid(_) | HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_) => None,
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
                    dst: Rect::new(
                        r.x() - pad,
                        y - pad,
                        r.width() + 2 * pad as u32,
                        r.height() + 2 * pad as u32,
                    ),
                    alpha: 0xff,
                });
            }
            if let Some(idx) = focused {
                // The focus pop: the GPU scales the (unfocused) card tile up
                // around its center as the pop progresses, with the shared ring
                // tile fading in over it at the same scale.
                let f = ui::anim_frac(self.focus_anim, ui::FOCUS_POP);
                let r = ui::grid_card_rect(idx, columns, grid_x, available_w);
                let y = r.y() - self.grid_scroll;
                let card_base = Rect::new(
                    r.x() - pad,
                    y - pad,
                    r.width() + 2 * pad as u32,
                    r.height() + 2 * pad as u32,
                );
                cmds.push(DrawCmd::Tex {
                    tile: Tile::Card(idx),
                    dst: ui::zoom_rect(card_base, f, CARD_GROWTH),
                    alpha: 0xff,
                });
                let rp = ui::FOCUS_RING_PAD;
                let ring_base = Rect::new(
                    r.x() - rp,
                    y - rp,
                    r.width() + 2 * rp as u32,
                    r.height() + 2 * rp as u32,
                );
                cmds.push(DrawCmd::Tex {
                    tile: Tile::Ring,
                    dst: ui::zoom_rect(ring_base, f, CARD_GROWTH),
                    alpha: (255.0 * f) as u8,
                });
            }
        }
        if self.selected_host.is_some() && self.home_status.is_some() {
            if let Some((_, p)) = &self.status_tile {
                // Fixed at two text rows regardless of how many the current message
                // actually wraps to, so the bar doesn't resize as status text changes.
                const STATUS_BG_PAD: i32 = 12;
                let line_h = fonts.label.height() + 6;
                let box_h = 2 * line_h as u32 + 2 * STATUS_BG_PAD as u32;
                let box_y = screen_h as i32 - box_h as i32;
                cmds.push(DrawCmd::Fill {
                    rect: Rect::new(grid_x, box_y, available_w, box_h),
                    color: ui::MODAL_SCRIM,
                });
                let y = box_y + (box_h as i32 - p.height() as i32) / 2;
                cmds.push(DrawCmd::Tex {
                    tile: Tile::Status,
                    dst: Rect::new(grid_x + ui::GRID_PAD, y, p.width(), p.height()),
                    alpha: 0xff,
                });
            }
        }

        let sidebar_focus_row = match self.home_focus {
            HomeFocus::Sidebar(i) | HomeFocus::SidebarMenu(i) => Some(i),
            HomeFocus::Grid(_) => None,
        };
        if let Some(i) = sidebar_focus_row {
            let rect = if i == self.entries.len() + 1 {
                ui::settings_row_rect(screen_h)
            } else {
                ui::sidebar_row_rect(i)
            };
            let pad = ui::ROW_TILE_PAD;
            cmds.push(DrawCmd::Tex {
                tile: Tile::FocusRow,
                dst: Rect::new(
                    rect.x() - pad,
                    rect.y() - pad,
                    rect.width() + 2 * pad as u32,
                    rect.height() + 2 * pad as u32,
                ),
                alpha: 0xff,
            });
        }

        if !matches!(self.screen, Screen::Home) {
            // Modal open: the scrim fades in and the modal slides up its last
            // ~26px while fading — both pure GPU parameters.
            let m = ui::anim_frac(self.modal_anim, MODAL_FADE);
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
            // Whichever modal's content overflows (Settings' rows, About's document),
            // computed once and reused by every block below instead of each
            // re-deriving it — see `scroll_geometry`'s docs.
            let scroll_geom = self.scroll_geometry(screen_w, screen_h, fonts);
            // Its content: cropped/repositioned straight off its own full (unscrolled)
            // tile — a GPU op, so scrolling never re-rasterizes anything (see
            // `Tile::ScrollContent`'s docs). About's tile only ever holds a bounded
            // window of the document, not the whole thing — `window_start` is where
            // that window begins, so the crop offset is relative to it, not to 0.
            if let Some((total, visible, _, content)) = scroll_geom {
                let scroll = self.scroll.clamped(total, visible);
                let window_start = match self.screen {
                    Screen::About => self.content_window.start,
                    _ => 0,
                };
                let stride = self.scroll_stride(fonts);
                cmds.push(DrawCmd::TexCropped {
                    tile: Tile::ScrollContent(self.screen),
                    src: Rect::new(
                        0,
                        (scroll - window_start) as i32 * stride,
                        content.width(),
                        content.height(),
                    ),
                    dst: Rect::new(content.x(), content.y() + dy, content.width(), content.height()),
                    alpha: (255.0 * m) as u8,
                });
            }
            // The open dropdown's panel + unfocused option list — Settings-only (no
            // other modal has one) — its own tile so it composites *after*
            // `Tile::ScrollContent` (which would otherwise redraw the rows the overlay
            // extends over, on top of it).
            if matches!(self.screen, Screen::Settings) {
                if let Some((total, visible, _, content)) = scroll_geom {
                    let scroll = self.scroll.clamped(total, visible);
                    if let Some(dd) = &self.dropdown {
                        let overlay_rect = Self::dropdown_overlay_rect(content, dd.row - scroll);
                        let options_len = ui::dropdown_options(&self.settings, dd.row).len();
                        cmds.push(DrawCmd::Tex {
                            tile: Tile::DropdownOverlay,
                            dst: Rect::new(
                                overlay_rect.x(),
                                overlay_rect.y() + dy,
                                overlay_rect.width(),
                                options_len as u32 * ui::DROPDOWN_OPTION_H,
                            ),
                            alpha: (255.0 * m) as u8,
                        });
                    }
                }
            }
            // Whichever modal is open, its one focused widget — a settings/Wake
            // row, a pairing digit/button, or a Forget-host button (see
            // `ModalFocusKey`'s docs) — composites on top of the shell (which
            // draws every widget unfocused) at its actual on-screen position,
            // so moving focus needs no modal re-rasterize at all. Same
            // fade/slide as the shell so it stays glued to it during the
            // modal-open animation.
            let focus_rect = match self.screen {
                Screen::Settings => {
                    let (total, visible, _, content) = scroll_geom.expect("screen is Screen::Settings");
                    let scroll = self.scroll.clamped(total, visible);
                    // `content`'s rows are the scrolled-to-visible window — translate
                    // the focused row's global index back to a local one.
                    Some(ui::focus_row_rect(content, self.settings_focused - scroll))
                }
                Screen::Wake => self.wake.as_ref().filter(|w| !w.mac.is_empty()).map(|w| {
                    let card = Self::wake_card_rect(screen_w, screen_h, w, fonts);
                    let toggle = Self::wake_toggle_rect(card, w, fonts);
                    if w.focused == 0 {
                        ui::focus_row_rect(toggle, 0)
                    } else {
                        ui::confirm_button_rect(Self::wake_buttons_rect(toggle), w.focused - 1)
                    }
                }),
                Screen::Pairing => {
                    let card = Self::pairing_card_rect(screen_w, screen_h, fonts);
                    Some(match self.pairing_focus {
                        PairingFocus::Pin => {
                            let digit_y = Self::pairing_pin_row_y(card, fonts);
                            ui::pairing_digit_rect(card, digit_y, self.pin_digit_index)
                        }
                        PairingFocus::RequestAccess => Self::pairing_request_button_rect(card, fonts),
                    })
                }
                Screen::ForgetHost => {
                    let name = self
                        .host_menu_index
                        .and_then(|i| self.entries.get(i))
                        .map(HostEntry::name)
                        .unwrap_or_default();
                    let card = Self::forget_host_card_rect(screen_w, screen_h, name, fonts);
                    let content = Self::forget_host_content_rect(card, name, fonts);
                    Some(ui::confirm_button_rect(content, self.host_menu_focused))
                }
                Screen::HostMenu => {
                    let subtitle = self.host_menu_subtitle();
                    let rows = self.host_menu_actions().len();
                    let card = Self::host_menu_card_rect(screen_w, screen_h, fonts, &subtitle, rows);
                    let content = ui::list_modal_content_rect(card, fonts, &subtitle, rows);
                    Some(ui::focus_row_rect(content, self.menu_focused))
                }
                Screen::SpeedTest => matches!(
                    self.speed_test,
                    Some(speedtest::SpeedTestState::Done { .. }) | Some(speedtest::SpeedTestState::Failed(_))
                )
                .then(|| {
                    let card = self.speed_test_card_rect(screen_w, screen_h, fonts);
                    ui::confirm_button_rect(self.speed_test_buttons_rect(card, fonts), self.speed_test_focused)
                }),
                Screen::Home | Screen::AddHost | Screen::EditHost | Screen::About => None,
            };
            if let Some(rect) = focus_rect {
                let pad = ui::ROW_TILE_PAD;
                let base = Rect::new(
                    rect.x() - pad,
                    rect.y() - pad + dy,
                    rect.width() + 2 * pad as u32,
                    rect.height() + 2 * pad as u32,
                );
                // The zoom-in: same GPU-scale-around-center technique as the
                // grid's card focus pop (see above) — `modal_focus_tile` is
                // rasterized once at its literal size, never re-rendered for
                // this (except while `switch_anim` animates its content, see
                // `prepare_tiles`).
                let f = ui::anim_frac(self.modal_focus_anim, ui::FOCUS_POP);
                cmds.push(DrawCmd::Tex {
                    tile: Tile::ModalFocusElement,
                    dst: ui::zoom_rect(base, f, 0.02),
                    alpha: (255.0 * m) as u8,
                });
            }
            // The open dropdown's focused option — same idea, composited on
            // top of the shell's unfocused option list at its actual
            // position, so navigating dropdown options needs no modal
            // re-rasterize either. Settings-only.
            if matches!(self.screen, Screen::Settings) {
                if let Some((total, visible, _, content)) = scroll_geom {
                    let scroll = self.scroll.clamped(total, visible);
                    if let Some(dd) = &self.dropdown {
                        let overlay_rect = Self::dropdown_overlay_rect(content, dd.row - scroll);
                        let option_rect = ui::dropdown_option_rect(overlay_rect, dd.focused);
                        cmds.push(DrawCmd::Tex {
                            tile: Tile::DropdownFocusOption,
                            dst: Rect::new(
                                option_rect.x(),
                                option_rect.y() + dy,
                                option_rect.width(),
                                option_rect.height(),
                            ),
                            alpha: (255.0 * m) as u8,
                        });
                    }
                }
            }
            // Whichever modal is scrollable, its indicator — full opacity for
            // `SCROLL_INDICATOR_HOLD`, then a linear fade over `SCROLL_INDICATOR_FADE`
            // (names kept from when only Settings had one; every scrollable modal now
            // shares the same timing and the same `self.scroll.shown_at` clock, since
            // only one is ever open at a time).
            if let Some((total, visible, card, content)) = scroll_geom {
                if total > visible {
                    let scroll_alpha = self.scroll.shown_at.map_or(0.0, |t| {
                        let elapsed = t.elapsed();
                        if elapsed < SCROLL_INDICATOR_HOLD {
                            1.0
                        } else {
                            let fading = (elapsed - SCROLL_INDICATOR_HOLD).as_secs_f32();
                            1.0 - (fading / SCROLL_INDICATOR_FADE.as_secs_f32()).clamp(0.0, 1.0)
                        }
                    });
                    if scroll_alpha > 0.0 {
                        // Sits nearer the card's edge than the content's, so it doesn't
                        // overlap a Settings row's dropdown pill/slider/switch. The `26`
                        // offset isn't derived from either modal's own width fraction —
                        // re-check both if either changes.
                        let dst = Rect::new(
                            card.x() + card.width() as i32 - 26,
                            content.y() + dy,
                            SCROLL_INDICATOR_TILE_W,
                            content.height(),
                        );
                        cmds.push(DrawCmd::Tex {
                            tile: Tile::ScrollIndicator(self.screen),
                            dst,
                            alpha: (255.0 * m * scroll_alpha) as u8,
                        });
                    }
                }
            }
        }
        cmds
    }

    /// Shared modal chrome — dark backdrop, the rounded card, and its close (X)
    /// button — every Settings/Pairing/AddHost/Wake screen draws exactly this
    /// before its own content inside `card`.
    pub(crate) fn draw_modal_shell(
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
}
