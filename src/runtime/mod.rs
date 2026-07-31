use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::config::Mode;
use sdl2::controller::GameController;

use crate::app::{App, HomeFocus, Screen, MODAL_FADE, MODAL_POP_SHRINK};
use crate::platform::webos::compositor::Compositor;
use crate::platform::webos::gamepad;
use crate::platform::webos::keyboard;
use crate::platform::webos::mouse;
use crate::services::store;
use crate::session;
use crate::ui::render::{DrawCmd, TileId as Tile};
use crate::ui::MenuEvent;

/// `ConnectOutcome`: connect thread (started early to overlap animation) + settings.
type ConnectOutcome = (std::thread::JoinHandle<Result<session::Connected>>, store::Settings);

/// Resolves a `GamepadType::Auto` preference against the attached controller, for this
/// session only.
///
/// Session-only on purpose: the returned `Settings` drives the handshake and the stream
/// loop, while `App`'s own copy (what `SettingsWriter` persists and what the Settings row
/// displays) keeps saying `Automatic`. Resolving into the stored value instead would turn
/// a preference that means "match my pad" into a fixed pad kind the next time a different
/// controller was plugged in.
fn resolve_gamepad_type(
    mut settings: store::Settings,
    game_controller: &sdl2::GameControllerSubsystem,
) -> store::Settings {
    if settings.gamepad_type != store::GamepadType::Auto {
        return settings;
    }
    if let Some(detected) = gamepad::detect_type(game_controller) {
        tracing::info!("controller Automatic → {detected:?} (mirroring the attached pad)");
        settings.gamepad_type = detected;
    }
    settings
}

/// Start `session::connect` on its own thread. Caller joins after animation (or immediately).
#[allow(clippy::too_many_arguments)]
fn spawn_connect(
    identity: (String, String),
    host: String,
    port: u16,
    fp: Option<[u8; 32]>,
    launch: Option<String>,
    settings: store::Settings,
    display_w: i32,
    display_h: i32,
) -> Result<std::thread::JoinHandle<Result<session::Connected>>> {
    std::thread::Builder::new()
        .name("punktfunk-webos-connect".into())
        .spawn(move || {
            // SDL2/Wayland reports refresh_rate=0; use settings' nominal rate instead
            let mode = Mode {
                width: settings.width,
                height: settings.height,
                refresh_hz: settings.refresh_hz,
            };
            tracing::info!(
                "requesting {}x{}@{}",
                settings.width,
                settings.height,
                settings.refresh_hz
            );
            session::connect(
                &host,
                port,
                mode,
                settings.bitrate_kbps,
                settings.hdr_enabled,
                settings.audio_channels,
                identity,
                fp,
                launch,
                // 185s: host parks unpinned/TOFU until approval (15s handhake budget too short)
                Duration::from_secs(185),
                display_w,
                display_h,
                settings.video_backend,
                settings.codec,
                settings.color_range_override,
                settings.video_pacing,
                settings.gamepad_type,
                settings.cursor_capture,
            )
        })
        .context("spawn connect thread")
}

/// Set by signal handler; read as extra quit condition (webOS uses SIGTERM before SIGKILL).
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe handler: just set the flag, cleanup happens at next poll.
extern "C" fn handle_term_signal(_signum: libc::c_int) {
    QUIT_REQUESTED.store(true, Ordering::Relaxed);
}

/// Install SIGTERM/SIGINT handlers (best-effort; failure uses OS default).
fn install_signal_handlers() {
    // SAFETY: function pointer matches libc::signal's documented safe shape
    unsafe {
        libc::signal(libc::SIGTERM, handle_term_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_term_signal as *const () as libc::sighandler_t);
    }
}

/// Yellow-button log overlay state (process-lifetime, all screens).
/// Explicit discriminants: `cycle_log_overlay` stores `next as u8` and
/// `log_overlay_state` decodes it — the two must agree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogOverlayState {
    Off = 0,
    /// Live tail — updates every refresh.
    Live = 1,
    /// Frozen snapshot for stable reading.
    Frozen = 2,
}

static LOG_OVERLAY_STATE: AtomicU8 = AtomicU8::new(0);
static FROZEN_LOG_LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn frozen_log_lines() -> &'static Mutex<Vec<String>> {
    FROZEN_LOG_LINES.get_or_init(|| Mutex::new(Vec::new()))
}

fn log_overlay_state() -> LogOverlayState {
    match LOG_OVERLAY_STATE.load(Ordering::Relaxed) {
        1 => LogOverlayState::Live,
        2 => LogOverlayState::Frozen,
        _ => LogOverlayState::Off,
    }
}

/// Yellow button cycle Off → Live → Frozen → Off; capture on/off at boundaries.
fn cycle_log_overlay() {
    let next = match log_overlay_state() {
        LogOverlayState::Off => {
            crate::logger::set_ring_capture(true);
            LogOverlayState::Live
        }
        LogOverlayState::Live => {
            let mut snap = frozen_log_lines().lock().unwrap_or_else(PoisonError::into_inner);
            *snap = crate::logger::recent_lines(crate::ui::LOG_OVERLAY_LINES);
            LogOverlayState::Frozen
        }
        LogOverlayState::Frozen => {
            crate::logger::set_ring_capture(false);
            LogOverlayState::Off
        }
    };
    LOG_OVERLAY_STATE.store(next as u8, Ordering::Relaxed);
}

/// Diagnostics' "Show logs" toggle, for remotes without a Yellow button. Unlike
/// `cycle_log_overlay`'s 3-state cycle this only ever lands on Off/Live; the
/// preference itself is persisted separately, in `Settings::show_logs`.
pub(crate) fn set_log_overlay_enabled(enabled: bool) {
    crate::logger::set_ring_capture(enabled);
    let next = if enabled {
        LogOverlayState::Live
    } else {
        LogOverlayState::Off
    };
    LOG_OVERLAY_STATE.store(next as u8, Ordering::Relaxed);
}

/// Current lines to render; None if Off.
fn log_overlay_lines() -> Option<Vec<String>> {
    match log_overlay_state() {
        LogOverlayState::Off => None,
        LogOverlayState::Live => Some(crate::logger::recent_lines(crate::ui::LOG_OVERLAY_LINES)),
        LogOverlayState::Frozen => Some(
            frozen_log_lines()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        ),
    }
}

pub fn run() -> Result<()> {
    install_signal_handlers();
    // Streams to a dev machine when `task deploy TELEMETRY=...` passed a
    // destination as a launch param; otherwise a versioned file under the app's
    // own writable directory (falls back to `/tmp` off-device, e.g. when
    // smoke-testing this binary on a Linux dev box before packaging). `_guard`
    // owns the background writer thread `non_blocking` spawns — held for the
    // whole process so logging never blocks a caller (in particular the
    // video-pump thread) on a slow disk or a dev machine not draining its
    // telemetry listener fast enough.
    let app_dir = store::app_dir();
    let _guard = crate::logger::init_subscriber(&app_dir).context("init logger")?;
    tracing::info!("punktfunk-webos starting");
    // Logged before anything else can fail: a report from a model neither developer
    // owns is only actionable if the log says what it was running on.
    crate::platform::webos::device::DeviceInfo::detect().log();

    // A panic on ANY thread otherwise goes only to stderr, which a SAM-launched
    // native app has no terminal for — the app simply vanishes back to the
    // launcher with nothing written down. Routing it through `tracing` puts the
    // message and location in the same log as everything else, which is the
    // difference between "it crashed" and a diagnosable report. (This catches Rust
    // panics only; a fault inside the vendor decode libraries kills the process
    // outright and is visible only as a log that stops mid-session.)
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(
            "PANIC on thread {:?}: {info}",
            std::thread::current().name().unwrap_or("unnamed"),
        );
        default_hook(info);
    }));

    // Errors from here on only ever reached stderr, which is invisible for a
    // webOS native app with no attached terminal.
    match run_inner() {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::error!("error: {e:#}");
            Err(e)
        }
    }
}

/// How long a controller shortcut ([`DisconnectChord`]) must be held before its dialog
/// opens — the in-stream disconnect dialog while streaming, the quit dialog in the menu.
/// Every button in these shortcuts is also real game input, so a hold — not a press — is
/// the only safe trigger (L1+R1 in particular is a common in-game bind); the hold window
/// is the margin against a stream dying mid-play. Shared by both loops so the remote's
/// held-Back EXIT gesture and the controller chord feel the same in either context —
/// 1s to match webOS's own long-press threshold on the EXIT gesture.
const EXIT_HOLD: Duration = Duration::from_millis(1000);

/// How long OK must be held on a focused Home game card to pin/unpin it instead
/// of launching it — see `pin_hold_gate`.
const PIN_HOLD: Duration = Duration::from_millis(500);

/// An in-flight hold-to-pin gesture: OK is down on a pinnable Home card. The
/// toggle fires the moment `PIN_HOLD` elapses (so the pin visibly lands under
/// the still-held button), and `fired` then makes the release a no-op instead
/// of the launch a quick tap would have dispatched.
struct PinHold {
    since: Instant,
    focus: HomeFocus,
    fired: bool,
}

/// The gamepad routes to the disconnect dialog (streaming) or quit dialog (menu): Guide,
/// both shoulders, or Start+Back, each held for [`EXIT_HOLD`].
///
/// Tracked as button state rather than read back from SDL because SDL only reports
/// transitions here — and a chord needs to know what is down *now*, not what changed
/// last. Three shortcuts share one timer: the gesture is "some disconnect chord has been
/// complete for long enough", so sliding from one chord into another (releasing Start
/// while both shoulders stay down) is one continuous hold rather than a restart.
#[derive(Default)]
struct DisconnectChord {
    guide: bool,
    left_shoulder: bool,
    right_shoulder: bool,
    start: bool,
    back: bool,
    /// When the currently-held chord became complete; `None` when none is.
    since: Option<Instant>,
}

impl DisconnectChord {
    /// Records one button transition and arms or disarms the hold timer.
    fn set(&mut self, button: sdl2::controller::Button, down: bool) {
        use sdl2::controller::Button;
        match button {
            Button::Guide => self.guide = down,
            Button::LeftShoulder => self.left_shoulder = down,
            Button::RightShoulder => self.right_shoulder = down,
            Button::Start => self.start = down,
            Button::Back => self.back = down,
            _ => return,
        }
        // Re-derived after every transition, so releasing any part of a chord restarts
        // the hold instead of leaving a stale deadline armed.
        self.since = match (self.complete(), self.since) {
            (true, Some(t)) => Some(t),
            (true, None) => Some(Instant::now()),
            (false, _) => None,
        };
    }

    fn complete(&self) -> bool {
        self.guide || (self.left_shoulder && self.right_shoulder) || (self.start && self.back)
    }

    /// Whether a chord has now been held long enough to fire.
    fn held_for(&self, hold: Duration) -> bool {
        self.since.is_some_and(|t| t.elapsed() >= hold)
    }

    /// Forgets all held buttons.
    ///
    /// Called when the chord fires and when the pad disconnects, because in both cases
    /// the releases that follow never reach [`set`](Self::set) — the open dialog swallows
    /// controller events, and an unplugged pad sends none. Without this the buttons would
    /// stay "down" forever and the dialog would reopen the instant it was dismissed.
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// What feeding an event to an open [`ConfirmDialog`] resolved to. `Confirmed`
/// leaves the dialog open — the caller runs its action and dismisses (or exits).
enum ConfirmAction {
    /// Primary (index 0) button activated.
    Confirmed,
    /// Cancel/Back — the close-fade has been started.
    Dismissed,
    /// Focus moved between buttons.
    Navigated,
}

/// A two-button confirm dialog (stop-streaming mid-stream, quit-app in the menu) —
/// same open/close fade as pre-stream modals. Rendered as a compositor overlay via
/// `Tile::DisconnectDialog` + `Tile::DisconnectFocusButton`; the menu and stream
/// never show one at the same time, so they share those two tile slots.
struct ConfirmDialog {
    title: &'static str,
    subtitle: &'static str,
    buttons: [crate::ui::ConfirmButton<'static>; 2],
    focus: Option<usize>,
    fade: crate::ui::ModalFade<usize>,
    /// Re-render only on open; focused button is its own tile.
    shell_dirty: bool,
    focus_dirty: bool,
    focus_anim: Option<Instant>,
    tc: crate::ui::TextCache,
}

impl ConfirmDialog {
    fn new(title: &'static str, subtitle: &'static str, buttons: [crate::ui::ConfirmButton<'static>; 2]) -> Self {
        Self {
            title,
            subtitle,
            buttons,
            focus: None,
            fade: crate::ui::ModalFade::new(),
            shell_dirty: false,
            focus_dirty: false,
            focus_anim: None,
            tc: crate::ui::TextCache::new(),
        }
    }

    fn is_open(&self) -> bool {
        self.focus.is_some()
    }

    /// Opens (or reopens) with `focus` focused.
    fn open(&mut self, focus: usize) {
        self.focus = Some(focus);
        self.fade.reopen();
        self.shell_dirty = true;
        self.focus_dirty = true;
        self.focus_anim = Some(Instant::now());
    }

    /// Moves focus between the two buttons (Left/Right while open).
    fn set_focus(&mut self, focus: usize) {
        self.focus = Some(focus);
        self.focus_dirty = true;
        self.focus_anim = Some(Instant::now());
    }

    /// Starts close-fade with the focused button.
    fn dismiss(&mut self) {
        if let Some(focus) = self.focus.take() {
            self.fade.close(focus);
        }
    }

    /// Returns `(focus, alpha, is_closing)` to draw, or `None` if nothing to show.
    fn frame(&self, dur: Duration) -> Option<(usize, f32, bool)> {
        if let Some((alpha, focus)) = self.fade.closing_frame(dur) {
            return Some((focus, alpha, true));
        }
        self.focus.map(|focus| (focus, self.fade.open_alpha(dur), false))
    }

    /// Feeds one SDL event to the open dialog. Fresh presses only, so an
    /// auto-repeating held key can't run an action twice. `None` when the event
    /// isn't the dialog's; `Confirmed` doesn't dismiss — the caller decides.
    fn handle_event(
        &mut self,
        event: &sdl2::event::Event,
        w: u32,
        h: u32,
        fonts: &crate::ui::Fonts,
    ) -> Option<ConfirmAction> {
        use sdl2::event::Event;
        let focus = self.focus?;
        // Magic Remote pointer: hovering a button focuses it, a click acts on it —
        // the same absolute button rects the dialog is drawn with, so it lines up
        // with what's on screen. `content` is a plain Rect (captured by copy), so
        // the closure holds no borrow of `self` that `set_focus` would collide with.
        let (_, content) = crate::ui::confirm_dialog_layout(w, h, fonts, self.subtitle);
        let button_at = |x: i32, y: i32| crate::ui::confirm_button_at(content, x, y);
        match *event {
            Event::MouseMotion { x, y, .. } => {
                return match button_at(x, y) {
                    Some(i) if i != focus => {
                        self.set_focus(i);
                        Some(ConfirmAction::Navigated)
                    }
                    _ => None,
                };
            }
            // Act on the button under the click; a click off both buttons is ignored
            // (the dialog stays open) rather than dismissing on a stray tap.
            Event::MouseButtonDown {
                mouse_btn: sdl2::mouse::MouseButton::Left,
                x,
                y,
                ..
            } => {
                return match button_at(x, y) {
                    Some(0) => Some(ConfirmAction::Confirmed),
                    Some(_) => {
                        self.dismiss();
                        Some(ConfirmAction::Dismissed)
                    }
                    None => None,
                };
            }
            _ => {}
        }
        let nav = match event {
            Event::KeyDown {
                keycode: Some(k),
                repeat: false,
                ..
            } => crate::platform::webos::input::menu_event_for_key(*k),
            Event::ControllerButtonDown { button, .. } => crate::platform::webos::input::menu_event_for_button(*button),
            _ => None,
        };
        match nav {
            Some(MenuEvent::Left | MenuEvent::Right) => {
                self.set_focus(1 - focus);
                Some(ConfirmAction::Navigated)
            }
            Some(MenuEvent::Confirm) if focus == 0 => Some(ConfirmAction::Confirmed),
            Some(MenuEvent::Confirm | MenuEvent::Back) => {
                self.dismiss();
                Some(ConfirmAction::Dismissed)
            }
            _ => None,
        }
    }

    /// Uploads any dirty tiles and appends this dialog's overlay (scrim + shell +
    /// popped focus button) for the current fade frame. No-op when nothing shows.
    fn draw(
        &mut self,
        compositor: &mut Compositor,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        fonts: &crate::ui::Fonts<'_>,
        w: u32,
        h: u32,
        cmds: &mut Vec<DrawCmd>,
    ) -> Result<()> {
        let Some((focus, m, closing)) = self.frame(MODAL_FADE) else {
            return Ok(());
        };
        let full = crate::ui::render::Rect::new(0, 0, w, h);
        if self.shell_dirty {
            self.shell_dirty = false;
            let shell = crate::ui::render_confirm_dialog_shell(w, h, fonts, self.title, self.subtitle, &self.buttons)?;
            compositor.upload(texture_creator, Tile::DisconnectDialog, &shell)?;
        }
        let (_, content) = crate::ui::confirm_dialog_layout(w, h, fonts, self.subtitle);
        let btn_rect = crate::ui::confirm_button_rect(content, focus);
        if self.focus_dirty {
            self.focus_dirty = false;
            let tile = crate::ui::render_confirm_button_tile(
                &mut self.tc,
                fonts,
                &self.buttons[focus],
                btn_rect.width(),
                btn_rect.height(),
            )?;
            compositor.upload(texture_creator, Tile::DisconnectFocusButton, &tile)?;
        }
        // Same open/close motion as the `App`'s `Screen` modals (see `draw_list`): slide
        // in from ~26px below while fading, and the shell scales up on open.
        let dy = ((1.0 - m) * 26.0) as i32;
        let pad = crate::ui::ROW_TILE_PAD;
        let base = crate::ui::render::Rect::new(
            btn_rect.x() - pad,
            btn_rect.y() - pad + dy,
            btn_rect.width() + 2 * pad as u32,
            btn_rect.height() + 2 * pad as u32,
        );
        let f = crate::ui::anim_frac(self.focus_anim, crate::ui::FOCUS_POP);
        let modal_base = crate::ui::render::Rect::new(0, dy, w, h);
        let shell_dst = if closing {
            modal_base
        } else {
            crate::ui::pop_in_rect(modal_base, m, MODAL_POP_SHRINK)
        };
        cmds.push(DrawCmd::Fill {
            rect: full,
            color: crate::ui::render::Color::RGBA(0, 0, 0, (f32::from(crate::ui::MODAL_SCRIM.a) * m) as u8),
        });
        cmds.push(DrawCmd::Tex {
            tile: Tile::DisconnectDialog,
            dst: shell_dst,
            alpha: (255.0 * m) as u8,
        });
        cmds.push(DrawCmd::Tex {
            tile: Tile::DisconnectFocusButton,
            dst: crate::ui::zoom_rect(base, f, 0.02),
            alpha: (255.0 * m) as u8,
        });
        Ok(())
    }
}

/// Rising-edge detect on the webOS EXIT gesture (a held Back, delivered as
/// `WEBOS_EXIT_SCANCODE` — polled since it's outside rust-sdl2's `Scancode` enum).
/// `prev` carries the last-frame state across calls.
fn exit_gesture_fired(prev: &mut bool) -> bool {
    let down = crate::platform::webos::input::webos_scancode_down(crate::platform::webos::input::WEBOS_EXIT_SCANCODE);
    let fired = down && !*prev;
    *prev = down;
    fired
}

/// webOS ships a real on-screen keyboard, and the SDL fork this app links wires it
/// up (`SDL_waylandwebos_osk.c` in `webosbrew/SDL-webOS`, driving `zwp_text_input_v3`)
/// — but only for an app that actually asks for text input. Nothing here ever called
/// `SDL_StartTextInput`, so the keyboard simply never appeared on the add-host screen
/// and the only way to enter an address was the remote's number pad.
///
/// `run_ui_flow` now starts text input whenever a screen that edits text is open and
/// stops it on the way out, and `SDL_SetTextInputRect` tells webOS where the field is
/// so the panel doesn't cover it. Committed text arrives as `Event::TextInput`.
fn text_input_screen(screen: Screen) -> bool {
    matches!(screen, Screen::AddHost | Screen::EditHost)
}

/// Edge-triggers Back off `held`: a repeat/OS-resent press while already held
/// produces nothing, so a single physical press dispatches Back exactly once no
/// matter how SDL reports (or misreports) repeats for it — e.g. a *held* Back
/// would otherwise cascade through every level of menu navigation in one go
/// (closing a dropdown, then the very next repeat exiting the screen it was on)
/// instead of stopping at the first. Shared by the menu loop's keyboard and
/// controller arms, which debounce identically.
fn edge_trigger_back(ev: Option<MenuEvent>, held: &mut bool) -> Option<MenuEvent> {
    if ev != Some(MenuEvent::Back) {
        return ev;
    }
    if *held {
        None
    } else {
        *held = true;
        ev
    }
}

/// The UI loop's input state that outlives a single event: the Back debounce,
/// an in-flight hold-to-pin, and analogue-stick nav.
#[derive(Default)]
struct UiInput {
    /// Whether a Back-mapped key/button is currently held, per the
    /// keyboard/gamepad event stream — edge-detected so a single physical press
    /// dispatches Back exactly once no matter how SDL reports (or misreports)
    /// repeats for it.
    menu_back_down: bool,
    /// Hold-to-pin on Home (see `PIN_HOLD`), while OK is held on a pinnable card.
    pin_held: Option<PinHold>,
    stick_nav: crate::platform::webos::input::StickMenuNav,
}

/// What the UI loop should do with the event `handle_ui_event` just consumed.
enum EventAction {
    /// Handled — carry on with the next event.
    Next,
    /// A launch is under way; leave the UI flow.
    Launch,
}

/// Hold-to-pin arbitration (see `PIN_HOLD`). `MenuEvent` has no press/release
/// notion, so the gesture works off raw SDL events: OK down on a pinnable Home
/// card starts the hold and is swallowed, and the launch can only ever come
/// from the release. `Some` means the event was the gesture's and goes no
/// further.
fn pin_hold_gate(
    app: &mut App,
    event: &sdl2::event::Event,
    input: &mut UiInput,
    display_mode: sdl2::video::DisplayMode,
    fonts: &crate::ui::Fonts,
    dirty: &mut bool,
) -> Option<EventAction> {
    use sdl2::event::Event;
    let (w, h) = (display_mode.w as u32, display_mode.h as u32);
    // The Magic Remote's pointer delivers OK as a left mouse button, so give it
    // the same hold-to-pin gesture the D-pad's Confirm has: a press on a hovered
    // pinnable Home card starts the hold and is swallowed (the pin fires on the
    // hold-elapsed tick, same as `PIN_HOLD` above), and the tap/launch comes only
    // from the release. A press on anything else falls through to the normal
    // click path.
    if let Event::MouseButtonDown {
        mouse_btn: sdl2::mouse::MouseButton::Left,
        x,
        y,
        ..
    } = *event
    {
        if !matches!(app.screen, Screen::Home) {
            return None;
        }
        // Land hover focus on the press point first — a button press can jostle the
        // remote off the last motion position.
        *dirty |= app.handle_mouse_motion(x, y, w, h, fonts);
        if input.pin_held.is_some() {
            return Some(EventAction::Next);
        }
        let columns = crate::ui::grid_columns(w.saturating_sub(crate::ui::SIDEBAR_W));
        if app.focused_pin_id(columns).is_some() {
            input.pin_held = Some(PinHold {
                since: Instant::now(),
                focus: app.home_focus,
                fired: false,
            });
            return Some(EventAction::Next);
        }
        return None;
    }
    // Release of a pointer OK: resolve whatever the matching press started. A fired
    // hold already pinned (swallow); a quick tap confirms whatever's under the
    // pointer now, exactly as an immediate click would have.
    if let Event::MouseButtonUp {
        mouse_btn: sdl2::mouse::MouseButton::Left,
        x,
        y,
        ..
    } = *event
    {
        let hold = input.pin_held.take()?;
        *dirty = true;
        if hold.fired {
            return Some(EventAction::Next);
        }
        return Some(if app.handle_mouse_click(x, y, w, h, fonts).is_some() {
            EventAction::Launch
        } else {
            EventAction::Next
        });
    }
    // No `repeat: false` filter, deliberately — OS auto-repeats while OK is held
    // have to be caught here too, not dispatched as fresh presses.
    let confirm_down = matches!(
        *event,
        Event::KeyDown { keycode: Some(k), .. }
            if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Confirm)
    ) || matches!(
        *event,
        Event::ControllerButtonDown { button, .. }
            if crate::platform::webos::input::menu_event_for_button(button) == Some(MenuEvent::Confirm)
    );
    if confirm_down {
        // OK stays the gesture's until released, whatever the toggle put on
        // screen: a hold that hit the pin limit opens the `PinLimit` alert
        // *under the still-held button*, and the next auto-repeat KeyDown would
        // otherwise dispatch Confirm to it and dismiss it instantly.
        if input.pin_held.is_some() {
            return Some(EventAction::Next);
        }
        let columns = crate::ui::grid_columns((display_mode.w as u32).saturating_sub(crate::ui::SIDEBAR_W));
        if matches!(app.screen, Screen::Home) && app.focused_pin_id(columns).is_some() {
            input.pin_held = Some(PinHold {
                since: Instant::now(),
                focus: app.home_focus,
                fired: false,
            });
            return Some(EventAction::Next);
        }
        return None;
    }
    let ends_hold = matches!(
        *event,
        Event::KeyUp { keycode: Some(k), .. }
            if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Confirm)
    ) || matches!(
        *event,
        Event::ControllerButtonUp { button, .. }
            if crate::platform::webos::input::menu_event_for_button(button) == Some(MenuEvent::Confirm)
    );
    // This press was ours (tap or hold) — swallow the release.
    let hold = ends_hold.then(|| input.pin_held.take()).flatten()?;
    *dirty = true;
    // A quick tap: the press never dispatched, so do it now. A hold that already
    // toggled, or one whose screen/focus moved out from under it, resolves to
    // nothing.
    let tapped = !hold.fired && matches!(app.screen, Screen::Home) && hold.focus == app.home_focus;
    let launched = tapped
        && app
            .handle_home_event(MenuEvent::Confirm, display_mode.w as u32, display_mode.h as u32)
            .is_some();
    Some(if launched {
        EventAction::Launch
    } else {
        EventAction::Next
    })
}

/// Routes a resolved `MenuEvent` to whichever screen is up — one of the
/// dispatch sites a new screen has to be added to.
fn dispatch_menu_event(
    app: &mut App,
    menu_ev: MenuEvent,
    display_mode: sdl2::video::DisplayMode,
    fonts: &crate::ui::Fonts,
) -> EventAction {
    let (w, h) = (display_mode.w as u32, display_mode.h as u32);
    if menu_ev == MenuEvent::Back {
        return if app.back().is_some() {
            EventAction::Launch
        } else {
            EventAction::Next
        };
    }
    match app.screen {
        Screen::Home => {
            if app.handle_home_event(menu_ev, w, h).is_some() {
                return EventAction::Launch;
            }
        }
        Screen::Pairing => app.handle_pairing_event(menu_ev),
        Screen::Settings => app.handle_settings_event(menu_ev, h),
        Screen::AddHost => app.handle_add_host_event(menu_ev),
        Screen::Wake => app.handle_wake_event(menu_ev),
        Screen::ForgetHost => app.handle_forget_host_event(menu_ev),
        Screen::HostMenu => app.handle_host_menu_event(menu_ev),
        Screen::WakeSettings => app.handle_wake_settings_event(menu_ev),
        Screen::SpeedTest => app.handle_speed_test_event(menu_ev),
        Screen::EditHost => app.handle_edit_host_event(menu_ev),
        Screen::About => app.handle_about_event(menu_ev, w, h, fonts),
        Screen::PinLimit => app.handle_pin_limit_event(menu_ev),
        Screen::Diagnostics => app.handle_diagnostics_event(menu_ev),
        Screen::Experimental => app.handle_experimental_event(menu_ev),
        Screen::SendLogs => app.handle_send_logs_event(menu_ev),
    }
    EventAction::Next
}

/// One SDL event from the pre-stream UI's pump, routed into `app`. `dirty` is
/// set whenever the event can have changed what's on screen. Device-level
/// events (quit, controller hotplug) are the caller's and never arrive here.
fn handle_ui_event(
    app: &mut App,
    event: sdl2::event::Event,
    input: &mut UiInput,
    display_mode: sdl2::video::DisplayMode,
    fonts: &crate::ui::Fonts,
    dirty: &mut bool,
) -> EventAction {
    use sdl2::event::Event;
    let (w, h) = (display_mode.w as u32, display_mode.h as u32);
    // The Magic Remote's pointer mode surfaces as a plain SDL2 MouseMotion
    // event fired continuously while the remote is moving — unlike every other
    // event handled below, redraw only if the motion actually changed the
    // focused/hovered element, not on every no-op tick.
    if let Event::MouseMotion { x, y, .. } = event {
        *dirty |= app.handle_mouse_motion(x, y, w, h, fonts);
        return EventAction::Next;
    }
    // The Magic Remote's scroll wheel — scrolls the game grid on Home (wheel
    // y > 0 = "scroll up" = content moves down). Like motion above, only
    // redraws when the offset actually moved (a wheel tick at either clamp
    // edge is a no-op).
    if let Event::MouseWheel { y: wheel_y, .. } = event {
        match app.screen {
            Screen::About => {
                /// Licence-wall px per wheel detent — a few lines at a time.
                const ABOUT_WHEEL_STEP: i32 = 90;
                *dirty |= app.scroll_about_by(-wheel_y * ABOUT_WHEEL_STEP, w, h, fonts);
            }
            Screen::Home => {
                /// Grid px scrolled per wheel detent — about a third of a card
                /// row, so a few ticks walk one row.
                const WHEEL_STEP: i32 = 120;
                *dirty |= app.scroll_grid_by(-wheel_y * WHEEL_STEP, w, h);
            }
            // List-modal screens (row-per-page, not pixel scroll): one detent
            // moves focus exactly one row, same as an Up/Down key press.
            Screen::Settings | Screen::HostMenu | Screen::WakeSettings | Screen::Diagnostics | Screen::Experimental
                if wheel_y != 0 =>
            {
                let menu_ev = if wheel_y > 0 { MenuEvent::Up } else { MenuEvent::Down };
                dispatch_menu_event(app, menu_ev, display_mode, fonts);
            }
            _ => {}
        }
        return EventAction::Next;
    }
    if let Some(action) = pin_hold_gate(app, &event, input, display_mode, fonts, dirty) {
        return action;
    }
    // Any other event might change what's on screen (focus/hover, a typed
    // digit, a screen transition) — simplest to mark dirty for all of them
    // rather than re-litigate that per event kind.
    *dirty = true;
    match event {
        // The Magic Remote's pointer delivers OK as a plain mouse click.
        // Dispatch it on press: there is no hold gesture to disambiguate any
        // more (per-host actions have their own ⋯ button — see
        // `ui::sidebar_menu_button_rect`), so nothing needs to wait for the
        // release.
        Event::MouseButtonDown {
            mouse_btn: sdl2::mouse::MouseButton::Left,
            x,
            y,
            ..
        } => {
            // A grid-card click resolves via `confirm_grid_card`'s async check,
            // same as a remote Confirm — never a target directly here.
            return if app.handle_mouse_click(x, y, w, h, fonts).is_some() {
                EventAction::Launch
            } else {
                EventAction::Next
            };
        }
        // Direct digit entry via the remote's number buttons — PIN entry on the
        // pairing screen, IP entry on the add/edit-host screens.
        Event::KeyDown { keycode: Some(k), .. }
            if matches!(app.screen, Screen::Pairing | Screen::AddHost | Screen::EditHost) =>
        {
            if let Some(digit) = crate::platform::webos::input::digit_key_value(k) {
                match app.screen {
                    Screen::Pairing => app.enter_pin_digit(digit),
                    Screen::AddHost | Screen::EditHost => app.enter_add_host_digit(digit),
                    _ => unreachable!(),
                }
                return EventAction::Next;
            }
        }
        // Text committed by webOS's on-screen keyboard (see `SOFTWARE_KEYBOARD`
        // in this module): the OSK delivers whole strings via SDL_TEXTINPUT, not
        // synthetic key events, so it has to be consumed separately from the
        // number-pad path above. Each character is fed through the same entry
        // state machine, so typing "192.168.1.5" on the keyboard and tapping it
        // out on the remote produce identical results.
        Event::TextInput { ref text, .. } => {
            match app.screen {
                Screen::Pairing => {
                    for d in text.chars().filter_map(|c| c.to_digit(10)) {
                        app.enter_pin_digit(d as u8);
                    }
                }
                Screen::AddHost | Screen::EditHost => {
                    for c in text.chars() {
                        app.enter_host_address_char(c);
                    }
                }
                _ => {}
            }
            return EventAction::Next;
        }
        _ => {}
    }
    let menu_ev = match event {
        Event::KeyDown { keycode: Some(k), .. } => edge_trigger_back(
            crate::platform::webos::input::menu_event_for_key(k),
            &mut input.menu_back_down,
        ),
        Event::KeyUp { keycode: Some(k), .. } => {
            if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) {
                input.menu_back_down = false;
            }
            None
        }
        Event::ControllerButtonDown { button, .. } => edge_trigger_back(
            crate::platform::webos::input::menu_event_for_button(button),
            &mut input.menu_back_down,
        ),
        Event::ControllerButtonUp { button, .. } => {
            if crate::platform::webos::input::menu_event_for_button(button) == Some(MenuEvent::Back) {
                input.menu_back_down = false;
            }
            None
        }
        Event::ControllerAxisMotion { axis, value, .. } => input.stick_nav.axis_event(axis, value),
        _ => None,
    };
    let Some(menu_ev) = menu_ev else {
        return EventAction::Next;
    };
    dispatch_menu_event(app, menu_ev, display_mode, fonts)
}

enum StreamOutcome {
    /// The system asked the app to close (not just this stream) — exit fully.
    Quit,
    /// The host ended the session, or the user held Back — go back to the
    /// host-list/settings UI instead of exiting the app.
    ReturnToMenu,
}

mod stream;
mod ui_flow;
use stream::run_inner;
use ui_flow::run_ui_flow;
