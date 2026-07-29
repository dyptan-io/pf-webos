//! Native webOS TV client for punktfunk (see `docs/NOTES.md` for architecture).
//! Platform-gated to `target_os` = "linux" (both webOS and Linux dev boxes).
#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod art;
#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod compositor;
#[cfg(target_os = "linux")]
mod device;
#[cfg(target_os = "linux")]
mod discovery;
#[cfg(target_os = "linux")]
mod errors;
#[cfg(target_os = "linux")]
mod gamepad;
#[cfg(target_os = "linux")]
mod keyboard;
#[cfg(target_os = "linux")]
mod library;
#[cfg(target_os = "linux")]
mod logger;
#[cfg(target_os = "linux")]
mod mouse;
#[cfg(target_os = "linux")]
mod ndl;
#[cfg(target_os = "linux")]
mod session;
#[cfg(target_os = "linux")]
mod starfish;
#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
mod ui;
#[cfg(target_os = "linux")]
mod wol;

#[cfg(target_os = "linux")]
mod real {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Mutex, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use punktfunk_core::config::Mode;
    use sdl2::controller::GameController;

    use crate::app::{App, HomeFocus, Screen, MODAL_FADE, MODAL_POP_SHRINK};
    use crate::compositor::{Compositor, DrawCmd, Tile};
    use crate::gamepad;
    use crate::keyboard;
    use crate::mouse;
    use crate::session;
    use crate::store;
    use crate::ui::MenuEvent;

    /// `ConnectOutcome`: connect thread (started early to overlap animation) + settings.
    type ConnectOutcome = (std::thread::JoinHandle<Result<session::Connected>>, store::Settings);

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
        crate::device::DeviceInfo::detect().log();

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

    /// How long the Xbox/PS "Guide" button must be held during a stream before the
    /// disconnect dialog opens. A plain Back/B press is real game input and must reach
    /// the host, so the gamepad's only route to the dialog is this deliberate hold.
    const GUIDE_HOLD: Duration = Duration::from_secs(2);

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

    /// In-stream disconnect dialog — same open/close fade as pre-stream modals.
    struct DisconnectDialog {
        focus: Option<usize>,
        fade: crate::ui::ModalFade<usize>,
        /// Re-render only on open; focused button is its own tile.
        shell_dirty: bool,
        focus_dirty: bool,
        focus_anim: Option<Instant>,
        tc: crate::ui::TextCache,
    }

    impl DisconnectDialog {
        fn new() -> Self {
            Self {
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

        /// Opens (or reopens) with `focus` focused — the Back key or a Guide hold.
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
        stick_nav: crate::ui::StickMenuNav,
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
        dirty: &mut bool,
    ) -> Option<EventAction> {
        use sdl2::event::Event;
        // No `repeat: false` filter, deliberately — OS auto-repeats while OK is held
        // have to be caught here too, not dispatched as fresh presses.
        let confirm_down = matches!(
            *event,
            Event::KeyDown { keycode: Some(k), .. }
                if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Confirm)
        ) || matches!(
            *event,
            Event::ControllerButtonDown { button, .. }
                if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Confirm)
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
                if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Confirm)
        ) || matches!(
            *event,
            Event::ControllerButtonUp { button, .. }
                if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Confirm)
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
                _ => {}
            }
            return EventAction::Next;
        }
        if let Some(action) = pin_hold_gate(app, &event, input, display_mode, dirty) {
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
                if let Some(digit) = crate::ui::digit_key_value(k) {
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
            Event::KeyDown { keycode: Some(k), .. } => {
                edge_trigger_back(crate::ui::menu_event_for_key(k), &mut input.menu_back_down)
            }
            Event::KeyUp { keycode: Some(k), .. } => {
                if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back) {
                    input.menu_back_down = false;
                }
                None
            }
            Event::ControllerButtonDown { button, .. } => {
                edge_trigger_back(crate::ui::menu_event_for_button(button), &mut input.menu_back_down)
            }
            Event::ControllerButtonUp { button, .. } => {
                if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Back) {
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

    /// Runs the UI (host list -> pairing -> settings) until the user confirms a
    /// connect target or the system asks the app to close (`None`). A plain
    /// function, not a closure — a closure capturing `canvas`/`events` by
    /// reference would hold that borrow for as long as the closure value exists,
    /// which conflicts with using them again in the streaming loop right after.
    #[allow(clippy::too_many_arguments)]
    fn run_ui_flow(
        canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
        compositor: &mut Compositor,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        events: &mut sdl2::EventPump,
        game_controller: &sdl2::GameControllerSubsystem,
        controller: &mut Option<GameController>,
        identity: &(String, String),
        display_mode: sdl2::video::DisplayMode,
        fonts: &crate::ui::Fonts,
        initial_status: Option<String>,
    ) -> Result<Option<ConnectOutcome>> {
        // Target period for this loop's render ticks, animating or not. Each active
        // (render) iteration used to sleep a flat 16ms *on top of* whatever the tick's own
        // work cost, so its real period was `work + 16ms` rather than 16ms — at a GIF frame
        // delay of ~33ms that was enough overshoot to occasionally miss a frame's window
        // and skip straight to the next one. Pacing off each tick's own start time keeps
        // the loop at a steady ~60Hz regardless of work cost, which comfortably samples
        // every 33ms spinner frame.
        const TICK_BUDGET: Duration = Duration::from_millis(16);
        // Test/dev override: skip the UI entirely if a connect.conf was dropped
        // alongside sideloading (see store.rs docs) — the UI flow is the normal path.
        // Bypasses the library screen too (`launch: None`, a plain desktop session).
        if let Some((host, port)) = store::dev_override_connect() {
            tracing::info!("dev override: connecting to {host}:{port}");
            let settings = store::load_settings();
            let handle = spawn_connect(
                identity.clone(),
                host,
                port,
                None,
                None,
                settings,
                display_mode.w,
                display_mode.h,
            )?;
            return Ok(Some((handle, settings)));
        }

        canvas.window_mut().show();
        let mut app = App::new(identity.clone());
        // Upload every spinner frame's GPU texture now, once, rather than letting each
        // frame's first appearance create it lazily inside the render loop. `upload_raw`
        // creates a *new* static texture (allocation, not just a pixel copy) the first
        // time a `Tile::SpinnerFrame(idx)` is seen — done inline during the animation
        // that meant the first spin cycle stalled once per unique frame, right when the
        // spinner is supposed to look smooth. `clear_all` (stream handoff) drops these
        // along with everything else, so this needs redoing on every re-entry here.
        for (idx, frame) in crate::ui::spinner_frames().iter().enumerate() {
            compositor.upload_raw(
                texture_creator,
                Tile::SpinnerFrame(idx),
                frame.width,
                frame.height,
                &frame.pixels,
            )?;
        }
        // E.g. "the last connect attempt failed, and here's why" — shown on the
        // Home screen the user just got dropped back onto (see `run_inner`'s
        // connect-error path).
        if initial_status.is_some() {
            app.home_status = initial_status;
        }
        // Rasterized-text cache (see `ui::TextCache` docs) — created once here and
        // threaded down through every render call for the rest of this UI-flow's
        // lifetime so repeat draws of the same (font, text, color) reuse an
        // already-rasterized+premultiplied `Pixmap` instead of re-rasterizing
        // freetype glyphs on every ~60fps tick.
        let mut text_cache = crate::ui::TextCache::new();
        let mut input = UiInput::default();
        // Owned handle (it just clones the video subsystem's refcount), so taking it
        // here doesn't hold a borrow on `canvas` for the rest of the loop.
        let text_input = canvas.window().subsystem().text_input();
        tracing::info!(
            "on-screen keyboard support: {}",
            text_input.has_screen_keyboard_support()
        );
        let mut text_input_active = false;
        // Redraw-on-change: outside a running animation (which the tick below asks
        // `App` about separately), pixels only ever change in reaction to an SDL
        // event or a discovery/art/library background result — anything else is a
        // no-op tick. Without this, `app.render(...)` (and the `canvas.present()`
        // vsync swap inside it) ran unconditionally every 16ms forever, even sitting
        // on an untouched menu. Starts `true` so the first frame always draws.
        let mut dirty = true;
        // Set once the reachability check passes — `spawn_connect` is already
        // running by then, so this just carries its handle out of the loop for
        // `run_inner` to join once the launch animation finishes.
        let mut connect_handle: Option<ConnectOutcome> = None;
        // Yellow button log overlay works here too (see streaming loop).
        let mut yellow_held = false;
        let mut log_overlay_last: Option<Instant> = None;
        // Cache last overlay tile size for idle frames (no re-render if size stable).
        let mut log_overlay_dims: Option<(u32, u32)> = None;
        'ui: loop {
            let tick_start = Instant::now();
            if QUIT_REQUESTED.load(Ordering::Relaxed) {
                tracing::warn!("SIGTERM/SIGINT received during UI");
                return Ok(None);
            }
            // Raw scancode poll (not SDL2 event); edge-detected like streaming loop.
            let yellow_down = crate::ui::webos_yellow_button_down();
            if yellow_down && !yellow_held {
                cycle_log_overlay();
                dirty = true; // force an immediate redraw with the new state
                log_overlay_last = None;
            }
            yellow_held = yellow_down;
            dirty |= app.drain_discovery();
            dirty |= app.drain_art();
            dirty |= app.drain_games();
            dirty |= app.drain_pairing();
            dirty |= app.drain_speed_test();
            dirty |= app.drain_send_logs();
            app.tick_reachability();
            dirty |= app.drain_reachability();
            dirty |= app.tick_wake();
            dirty |= app.drain_launch_check();
            // Fire on hold elapsed, not release, so user sees it before letting go.
            if let Some(hold) = input
                .pin_held
                .as_mut()
                .filter(|h| !h.fired && h.since.elapsed() >= PIN_HOLD)
            {
                hold.fired = true;
                let still_there = matches!(app.screen, Screen::Home) && hold.focus == app.home_focus;
                if still_there {
                    app.toggle_focused_pin(display_mode.w as u32, display_mode.h as u32);
                }
                dirty = true;
            }
            // Start connect in parallel with launch anim (fast handshake finishes first).
            if app.launch_ready.is_some() && connect_handle.is_none() {
                app.launch_anim = Some(Instant::now());
                dirty = true;
                if let Some(target) = app.take_ready_launch() {
                    let settings = store::load_settings();
                    let handle = spawn_connect(
                        identity.clone(),
                        target.host,
                        target.port,
                        Some(target.fingerprint),
                        target.launch,
                        settings,
                        display_mode.w,
                        display_mode.h,
                    )?;
                    connect_handle = Some((handle, settings));
                }
            }
            if app.launch_anim.is_some_and(|t| t.elapsed() >= crate::ui::LAUNCH_FADE) {
                break 'ui;
            }
            for event in events.poll_iter() {
                use sdl2::event::Event;
                // Device-level events, handled before anything screen-specific:
                // shutdown and controller hotplug.
                match event {
                    Event::Quit { .. } => {
                        tracing::info!("quit during UI");
                        return Ok(None);
                    }
                    Event::ControllerDeviceAdded { which, .. } if controller.is_none() => {
                        match game_controller.open(which) {
                            Ok(c) => {
                                tracing::info!("controller connected: {}", c.name());
                                *controller = Some(c);
                            }
                            Err(e) => tracing::warn!("controller open failed: {e}"),
                        }
                        continue;
                    }
                    Event::ControllerDeviceRemoved { .. } => {
                        *controller = None;
                        continue;
                    }
                    _ => {}
                }
                match handle_ui_event(&mut app, event, &mut input, display_mode, fonts, &mut dirty) {
                    EventAction::Next => {}
                    EventAction::Launch => break 'ui,
                }
            }
            // Track actual keyboard state (user can dismiss while field focused; moves card).
            let keyboard_shown = text_input.is_screen_keyboard_shown(canvas.window());
            if keyboard_shown != app.keyboard_shown {
                app.keyboard_shown = keyboard_shown;
                dirty = true;
                tracing::debug!("on-screen keyboard shown: {keyboard_shown}");
            }
            // Toggle text input (edge-triggered; SDL doesn't tolerate repeated calls).
            let wants_text = text_input_screen(app.screen);
            if wants_text != text_input_active {
                text_input_active = wants_text;
                if wants_text {
                    text_input.set_rect(app.address_field_rect(display_mode.w as u32, display_mode.h as u32, fonts));
                    text_input.start();
                } else {
                    text_input.stop();
                }
                // Log both; separate SDL callbacks — some drivers implement only one.
                tracing::debug!(
                    "text input requested: {wants_text} (keyboard shown: {})",
                    text_input.is_screen_keyboard_shown(canvas.window())
                );
            }
            // Five reasons to render: dirty, animations running, tiles pending,
            // spinner animating, or log overlay due for refresh (~2Hz).
            // 16ms sleep when none holds keeps SoC idle.
            let animating = app.tick_animations() || app.tiles_pending || !app.grid_reveal_ready;
            let log_overlay_due = log_overlay_state() != LogOverlayState::Off
                && log_overlay_last.is_none_or(|t| t.elapsed() >= Duration::from_millis(500));
            if !dirty && !animating && !log_overlay_due {
                let elapsed = tick_start.elapsed();
                if elapsed < TICK_BUDGET {
                    std::thread::sleep(TICK_BUDGET - elapsed);
                }
                continue;
            }
            let content_dirty = dirty;
            dirty = false;
            let updated = app.prepare_tiles(
                &mut text_cache,
                fonts,
                display_mode.w as u32,
                display_mode.h as u32,
                content_dirty,
            )?;
            // Free old textures before uploading new (reduce peak memory during scroll).
            for tile in std::mem::take(&mut app.evicted_tiles) {
                compositor.drop_tile(tile);
            }
            for tile in updated {
                match &tile {
                    &Tile::SpinnerFrame(idx) => {
                        if let Some(frame) = crate::ui::spinner_frame(idx) {
                            compositor.upload_raw(texture_creator, tile, frame.width, frame.height, &frame.pixels)?;
                        }
                    }
                    _ => {
                        if let Some(pm) = app.tile_pixmap(&tile) {
                            compositor.upload(texture_creator, tile, pm)?;
                        }
                    }
                }
            }
            let mut cmds = app.draw_list(display_mode.w as u32, display_mode.h as u32, fonts);
            // Appended into the same single draw list/present as the rest of the
            // screen — this loop has no separate overlay pass (see the streaming
            // loop's `Tile::LogOverlay` handling for why that one differs).
            //
            // Text is only re-rendered/re-uploaded when `log_overlay_due` (~2Hz) —
            // otherwise every animation tick (scroll, focus pop, hover) while the
            // overlay is on would re-rasterize and re-upload it on every single
            // frame instead of twice a second, which is what made the menu feel
            // laggy with the overlay enabled (the streaming loop already gated
            // this correctly; this one didn't).
            if let Some(lines) = log_overlay_lines() {
                if log_overlay_due {
                    log_overlay_last = Some(Instant::now());
                    match crate::ui::render_log_overlay_tile(fonts.caption, display_mode.w as u32, &lines) {
                        Ok(tile) => {
                            log_overlay_dims = Some((tile.width(), tile.height()));
                            compositor.upload(texture_creator, Tile::LogOverlay, &tile)?;
                        }
                        Err(e) => tracing::warn!("log overlay render failed: {e:#}"),
                    }
                }
                if let Some((tw, th)) = log_overlay_dims {
                    cmds.push(DrawCmd::Tex {
                        tile: Tile::LogOverlay,
                        dst: sdl2::rect::Rect::new(0, display_mode.h - th as i32, tw, th),
                        alpha: 0xff,
                    });
                }
            }
            canvas.set_blend_mode(sdl2::render::BlendMode::None);
            canvas.set_draw_color(crate::ui::BG);
            canvas.clear();
            compositor.execute(canvas, &cmds)?;
            canvas.present();
            let elapsed = tick_start.elapsed();
            if elapsed < TICK_BUDGET {
                std::thread::sleep(TICK_BUDGET - elapsed);
            }
        }
        if text_input_active {
            text_input.stop();
        }
        Ok(connect_handle)
    }

    fn run_inner() -> Result<()> {
        // Prevents webOS's system launcher from intercepting the Magic Remote's Back
        // key, a connected HID keyboard's Windows/Meta key, and a gamepad's Guide
        // button (which webOS otherwise treats as its own Home shortcut, backgrounding
        // the app into the launcher — see `keyboard.rs`'s LGui/RGui mapping and
        // `gamepad.rs`'s BTN_GUIDE mapping, which need these to actually reach the app
        // instead). Must be set before window creation — confirmed on-device these
        // hints only latch at creation time, so there's no way to scope them to just
        // the stream: the tradeoff is the remote's own physical Home button no longer
        // opens webOS's launcher either, in the menu or mid-stream (accepted — the
        // priority is that no keyboard/gamepad input can ever reach the TV OS, only
        // the Magic Remote can).
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "true");
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_HOME", "true");
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_META", "true");
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_GUIDE", "true");
        // The hints above stop the key itself from backgrounding the app, but webOS's
        // card-switcher ribbon overlay is gated separately — without this it can still
        // pop the launcher UI on top even though the app stays foregrounded (confirmed
        // pairing in aurora-tv's app.c).
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_RIBBON", "false");
        // Linear texture filtering (SDL defaults to nearest) — the focus pop
        // scales card textures slightly, which shimmers without it.
        sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "1");
        let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL_Init: {e}"))?;
        let ttf = sdl2::ttf::init().map_err(|e| anyhow::anyhow!("SDL_ttf init: {e}"))?;
        let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video subsystem: {e}"))?;
        let game_controller = sdl
            .game_controller()
            .map_err(|e| anyhow::anyhow!("SDL game controller subsystem: {e}"))?;
        let sdl_audio = sdl.audio().map_err(|e| anyhow::anyhow!("SDL audio subsystem: {e}"))?;
        tracing::info!("SDL video subsystem up (driver: {})", video.current_video_driver());

        let display_mode = video
            .current_display_mode(0)
            .map_err(|e| anyhow::anyhow!("current_display_mode: {e}"))?;
        tracing::info!(
            "display mode: {}x{}@{}",
            display_mode.w,
            display_mode.h,
            display_mode.refresh_rate
        );

        let window = video
            .window("punktfunk", display_mode.w as u32, display_mode.h as u32)
            .fullscreen()
            .build()
            .map_err(|e| anyhow::anyhow!("create window: {e}"))?;
        let mut canvas = window
            .into_canvas()
            .build()
            .map_err(|e| anyhow::anyhow!("create canvas: {e}"))?;
        let texture_creator = canvas.texture_creator();
        tracing::info!("window + canvas created (renderer: {})", canvas.info().name);

        // The pre-stream UI's rendering backend: tiny-skia rasterizes cached
        // widget tiles (see `ui.rs`'s `render_*_tile` helpers), and the GPU
        // (`opengles2`, confirmed live on-device) composites them each frame via
        // this compositor — see `compositor.rs`'s module docs.
        let mut compositor = Compositor::new();

        let mut events = sdl.event_pump().map_err(|e| anyhow::anyhow!("event pump: {e}"))?;

        let identity = store::load_or_create_identity().context("load_or_create_identity")?;

        // Sized for a 10-foot TV viewing distance — see ui.rs's ROW_H/ROW_MAX_W docs.
        let font_label = crate::ui::load_font(&ttf, display_mode.h as u32, 22, crate::ui::FontWeight::Medium)?;
        let font_value = crate::ui::load_font(&ttf, display_mode.h as u32, 20, crate::ui::FontWeight::Regular)?;
        let font_title = crate::ui::load_font(&ttf, display_mode.h as u32, 40, crate::ui::FontWeight::SemiBold)?;
        let font_caption = crate::ui::load_font(&ttf, display_mode.h as u32, 14, crate::ui::FontWeight::Regular)?;
        let icon_font = crate::ui::load_icon_font(&ttf)?;
        let fonts = crate::ui::Fonts {
            label: &font_label,
            value: &font_value,
            title: &font_title,
            icon: &icon_font,
            caption: &font_caption,
        };

        // Owned here, at the top of the menu/stream cycle, rather than re-declared in
        // each: `ControllerDeviceAdded` only fires once per physical (re)connection, so
        // a pad already open from the menu (or a previous stream) needs to carry
        // straight through a screen transition instead of waiting for a replug neither
        // side will ever see.
        let mut controller: Option<GameController> = None;
        // Carried across the loop: why the *last* stream attempt bounced back to
        // the menu (connect/audio failure), surfaced as the fresh Home screen's
        // status line.
        let mut menu_status: Option<String> = None;

        loop {
            let Some((connect_thread, settings)) = run_ui_flow(
                &mut canvas,
                &mut compositor,
                &texture_creator,
                &mut events,
                &game_controller,
                &mut controller,
                &identity,
                display_mode,
                &fonts,
                menu_status.take(),
            )?
            else {
                tracing::info!("punktfunk-webos exiting cleanly");
                return Ok(());
            };
            tracing::debug!("settings: {settings:?}");

            // `hide()` (the previous approach here, when `set_opacity` fails — confirmed
            // unsupported on this Wayland backend) unmaps the surface entirely, which
            // stops it receiving pointer focus/motion at all — silently breaking the
            // Magic Remote pointer → host-mouse forwarding above, since there's no
            // mapped surface left for Wayland to route those events to (still fine for
            // keyboard-style remote-key polling, which webOS seems to route by
            // foreground app identity rather than surface focus). aurora-tv (the same
            // NDL punch-through technique, with its own working pointer support) never
            // hides its window at all — it stays mapped, just cleared fully transparent
            // each frame so the video plane underneath shows through. Doing the same
            // here: one transparent clear, window stays visible/mapped.
            canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
            canvas.clear();
            canvas.present();
            // Release all UI GPU textures (spinner frames, card art, sidebar …)
            // before the stream takes the GPU. The compositor is re-populated
            // from scratch when the user returns to the menu.
            tracing::debug!("releasing all compositor textures for stream handoff");
            compositor.clear_all();
            // The system draws its own cursor (a real SDL2 cursor this fork loads from
            // `/usr/share/im/...` — confirmed via `SDL_waylandwebos_cursor.c`) tracking
            // the physical remote directly; the host draws a second, independent one
            // wherever our forwarded `MouseMoveAbs` puts it. Two visible cursors reads
            // as "the pointer doesn't match the remote" — hide the local one so only
            // the host's shows. Restored when back in the menu (`sdl.mouse()` is the
            // same standard SDL2 API on any platform, not webOS-specific).
            sdl.mouse().show_cursor(false);

            // Already running (started back in `run_ui_flow`, overlapping the launch
            // zoom/fade) — joining just waits out whatever's left of the handshake,
            // which for a fast local connect is often nothing at all.
            let connected = match connect_thread.join().expect("connect thread panicked") {
                Ok(c) => c,
                Err(e) => {
                    // A failed connect (host went down in the race, codec/launch
                    // rejection, handshake error) used to `?` out of `run_inner`
                    // and take the whole app down — return to the menu with the
                    // reason on screen instead.
                    tracing::error!("session connect failed: {e:#}");
                    sdl.mouse().show_cursor(true);
                    menu_status = Some(format!("Couldn't connect: {}", crate::errors::friendly(&e)));
                    continue;
                }
            };
            tracing::info!("session connected, entering event loop");

            // Skipped entirely when NDL took the Opus stream: opening a second audio
            // device that nothing ever feeds would still claim a PulseAudio sink.
            let mut audio_player = if connected.audio_offloaded {
                None
            } else {
                match crate::audio::AudioPlayer::new(&sdl_audio, connected.client.audio_channels) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        // Same no-crash policy as the connect above — including the
                        // video-side teardown the normal stream exit does, since the
                        // connect succeeded and loaded a decoder.
                        tracing::error!("audio player init failed: {e:#}");
                        connected.client.disconnect_quit();
                        connected.shutdown();
                        crate::ndl::quit();
                        sdl.mouse().show_cursor(true);
                        menu_status = Some(format!("Couldn't start audio: {e:#}"));
                        continue;
                    }
                }
            };
            if let Some(player) = &audio_player {
                tracing::info!(
                    "SDL audio driver: {}, spec: {:?}",
                    sdl_audio.current_audio_driver(),
                    player.spec()
                );
            }

            let mut scroll_acc = mouse::ScrollAccumulator::default();
            // In-stream stats overlay: refreshed at ~2Hz onto the otherwise-transparent
            // stream window, composited OVER the punch-through video plane via the
            // surface's per-pixel alpha. The window is never shown/hidden here (that's
            // what crashed the old overlay attempt — see docs/NOTES.md). Starts from the
            // Settings-screen default; the Green button below flips it live for the rest
            // of this stream only, without writing back to `settings`.
            let mut stats_enabled = settings.stats_overlay;
            let mut green_held = false;
            let mut yellow_held = false;
            let mut overlay_last: Option<Instant> = None;
            let mut overlay_prev_frames: u64 = 0;
            let mut overlay_prev_bytes: u64 = 0;
            let mut overlay_prev_cpu_ticks: Option<u64> = None;
            let mut overlay_prev_at = Instant::now();
            // 0 = "Disconnect" focused, 1 = "Cancel" (default on open — safer).
            let mut disconnect = DisconnectDialog::new();
            // Gamepad path to the disconnect dialog (see `GUIDE_HOLD`): `Some(t)` =
            // Guide pressed at `t` and still held; cleared on release or once it fires.
            let mut guide_held_since: Option<Instant> = None;
            // Delayed outcome: waits for close-fade to finish.
            let mut pending_outcome: Option<StreamOutcome> = None;
            let outcome = 'running: loop {
                if QUIT_REQUESTED.load(Ordering::Relaxed) {
                    tracing::warn!("SIGTERM/SIGINT received — disconnecting before exit");
                    connected.client.disconnect_quit();
                    break 'running StreamOutcome::Quit;
                }
                for event in events.poll_iter() {
                    use sdl2::event::Event;
                    match event {
                        Event::Quit { .. } => {
                            connected.client.disconnect_quit();
                            break 'running StreamOutcome::Quit;
                        }
                        Event::ControllerDeviceAdded { which, .. } if controller.is_none() => {
                            match game_controller.open(which) {
                                Ok(c) => {
                                    tracing::info!("controller connected: {}", c.name());
                                    controller = Some(c);
                                }
                                Err(e) => tracing::warn!("controller open failed: {e}"),
                            }
                        }
                        Event::ControllerDeviceRemoved { .. } => {
                            controller = None;
                        }
                        // Dialog open: navigate it only, don't forward input to the
                        // host. Non-repeat keys/fresh controller presses only, so the
                        // held Back that opened it doesn't also dismiss it.
                        _ if disconnect.is_open() => {
                            let focus = disconnect.focus.expect("guarded by is_open above");
                            let nav = match &event {
                                Event::KeyDown {
                                    keycode: Some(k),
                                    repeat: false,
                                    ..
                                } => crate::ui::menu_event_for_key(*k),
                                Event::ControllerButtonDown { button, .. } => crate::ui::menu_event_for_button(*button),
                                _ => None,
                            };
                            match nav {
                                Some(MenuEvent::Left) | Some(MenuEvent::Right) => {
                                    disconnect.set_focus(1 - focus);
                                }
                                Some(MenuEvent::Confirm) if focus == 0 => {
                                    tracing::info!("back — disconnecting to menu");
                                    connected.client.disconnect_quit();
                                    disconnect.dismiss();
                                    pending_outcome = Some(StreamOutcome::ReturnToMenu);
                                }
                                Some(MenuEvent::Confirm) | Some(MenuEvent::Back) => {
                                    disconnect.dismiss();
                                    overlay_last = None;
                                }
                                _ => {}
                            }
                        }
                        // Scancode keys are real game input (Backspace/Escape/etc.
                        // included) — forward only, never open the dialog.
                        Event::KeyDown { scancode: Some(sc), .. } => {
                            if let Some(ev) = keyboard::key_event(sc, true) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        // Magic Remote Back (0x200003): no scancode, never
                        // forwarded to the host — open the disconnect dialog.
                        Event::KeyDown {
                            keycode: Some(k),
                            scancode: None,
                            repeat: false,
                            ..
                        } if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back) => {
                            disconnect.open(1);
                        }
                        Event::KeyUp { scancode: Some(sc), .. } => {
                            if let Some(ev) = keyboard::key_event(sc, false) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        Event::ControllerButtonDown { button, .. } => {
                            if button == sdl2::controller::Button::Guide {
                                guide_held_since = Some(Instant::now());
                            }
                            let ev = gamepad::button_event(button, true, 0);
                            let _ = session::send_input(&connected.client, &ev);
                        }
                        Event::ControllerButtonUp { button, .. } => {
                            if button == sdl2::controller::Button::Guide {
                                guide_held_since = None;
                            }
                            let ev = gamepad::button_event(button, false, 0);
                            let _ = session::send_input(&connected.client, &ev);
                        }
                        Event::ControllerAxisMotion { axis, value, .. } => {
                            let ev = gamepad::axis_event(axis, value, 0);
                            let _ = session::send_input(&connected.client, &ev);
                        }
                        // The Magic Remote's pointer mode surfaces as plain SDL2 mouse
                        // events (same as the pre-stream menu's hover/click) — forwarded
                        // to the host as real HID mouse input during a stream instead of
                        // driving local UI focus (see `mouse.rs`).
                        Event::MouseMotion { x, y, .. } => {
                            let ev = mouse::move_event(x, y, display_mode.w as u32, display_mode.h as u32);
                            let _ = session::send_input(&connected.client, &ev);
                        }
                        Event::MouseButtonDown { mouse_btn, .. } => {
                            if let Some(ev) = mouse::button_event(mouse_btn, true) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        Event::MouseButtonUp { mouse_btn, .. } => {
                            if let Some(ev) = mouse::button_event(mouse_btn, false) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        Event::MouseWheel { x, y, .. } => {
                            if y != 0 {
                                if let Some(ev) = scroll_acc.scroll_event(y, false) {
                                    let _ = session::send_input(&connected.client, &ev);
                                }
                            }
                            if x != 0 {
                                if let Some(ev) = scroll_acc.scroll_event(x, true) {
                                    let _ = session::send_input(&connected.client, &ev);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                // Guide held long enough (and the dialog isn't already up) —
                // open it, then disarm so it fires once per hold.
                if !disconnect.is_open() {
                    if let Some(since) = guide_held_since {
                        if since.elapsed() >= GUIDE_HOLD {
                            guide_held_since = None;
                            disconnect.open(1);
                        }
                    }
                }
                // Green button: local-only stats-overlay toggle, edge-detected here
                // (not via the event queue — see `ui::webos_green_button_down`'s docs
                // on why the safe SDL2 event API can't see this key at all). Skipped
                // while the disconnect dialog owns input, same as scancode forwarding.
                let green_down = !disconnect.is_open() && crate::ui::webos_green_button_down();
                if green_down && !green_held {
                    stats_enabled = !stats_enabled;
                    if stats_enabled {
                        overlay_last = None; // force an immediate redraw
                    } else {
                        // Nothing else clears the canvas once the overlay stops
                        // drawing — wipe both buffers back to transparent so the
                        // last frame doesn't stick over the video.
                        canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                        canvas.clear();
                        canvas.present();
                        canvas.clear();
                        canvas.present();
                    }
                }
                green_held = green_down;
                // Yellow button: log-tail overlay Off -> Live -> Frozen -> Off, same
                // edge-detect technique as Green above (raw scancode poll — see
                // `ui::webos_yellow_button_down`'s docs). Works on every screen, not
                // just while streaming — see the matching handling in `run_ui_flow`.
                let yellow_down = !disconnect.is_open() && crate::ui::webos_yellow_button_down();
                if yellow_down && !yellow_held {
                    cycle_log_overlay();
                    overlay_last = None; // force an immediate redraw with the new state
                    if log_overlay_state() == LogOverlayState::Off && !stats_enabled {
                        // Same "nothing else clears this canvas" wipe as Green's
                        // toggle-off above — otherwise the last overlay frame sticks.
                        canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                        canvas.clear();
                        canvas.present();
                        canvas.clear();
                        canvas.present();
                    }
                }
                yellow_held = yellow_down;
                // Captured once: reused below to skip the stats overlay for exactly the
                // ticks the dialog block itself owns the canvas — that's wider than
                // `is_open()`, since a dismissed dialog still draws (fading out) for a
                // few more ticks after `focus` has already gone back to `None`.
                let dialog_frame = disconnect.frame(MODAL_FADE);
                if let Some((focus, m, closing)) = dialog_frame {
                    let full = sdl2::rect::Rect::new(0, 0, display_mode.w as u32, display_mode.h as u32);
                    if disconnect.shell_dirty {
                        disconnect.shell_dirty = false;
                        let shell = crate::ui::render_disconnect_dialog_shell(full.width(), full.height(), &fonts)?;
                        compositor.upload(&texture_creator, Tile::DisconnectDialog, &shell)?;
                    }
                    let (_, content) = crate::ui::disconnect_dialog_layout(full.width(), full.height(), fonts.label);
                    let btn_rect = crate::ui::confirm_button_rect(content, focus);
                    if disconnect.focus_dirty {
                        disconnect.focus_dirty = false;
                        let buttons = crate::ui::disconnect_dialog_buttons();
                        let tile = crate::ui::render_confirm_button_tile(
                            &mut disconnect.tc,
                            &fonts,
                            &buttons[focus],
                            btn_rect.width(),
                            btn_rect.height(),
                        )?;
                        compositor.upload(&texture_creator, Tile::DisconnectFocusButton, &tile)?;
                    }
                    // The zoom-in: same GPU-scale-around-center technique as
                    // every other modal's focused widget (see `app.rs`'s
                    // `draw_list`) — `Tile::DisconnectFocusButton` is
                    // rasterized once, at its literal size, never re-rendered
                    // for this. Independent of the dialog's own open/close fade.
                    let pad = crate::ui::ROW_TILE_PAD;
                    let base = sdl2::rect::Rect::new(
                        btn_rect.x() - pad,
                        btn_rect.y() - pad,
                        btn_rect.width() + 2 * pad as u32,
                        btn_rect.height() + 2 * pad as u32,
                    );
                    let f = crate::ui::anim_frac(disconnect.focus_anim, crate::ui::FOCUS_POP);
                    // Open grows in like every other modal; close stays a plain fade,
                    // no scale — see `app.rs`'s `draw_list` for why.
                    let shell_dst = if closing {
                        full
                    } else {
                        crate::ui::pop_in_rect(full, m, MODAL_POP_SHRINK)
                    };
                    canvas.set_blend_mode(sdl2::render::BlendMode::None);
                    canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                    canvas.clear();
                    compositor.execute(
                        &mut canvas,
                        &[
                            DrawCmd::Fill {
                                rect: full,
                                color: sdl2::pixels::Color::RGBA(
                                    0,
                                    0,
                                    0,
                                    (f32::from(crate::ui::MODAL_SCRIM.a) * m) as u8,
                                ),
                            },
                            DrawCmd::Tex {
                                tile: Tile::DisconnectDialog,
                                dst: shell_dst,
                                alpha: (255.0 * m) as u8,
                            },
                            DrawCmd::Tex {
                                tile: Tile::DisconnectFocusButton,
                                dst: crate::ui::zoom_rect(base, f, 0.02),
                                alpha: (255.0 * m) as u8,
                            },
                        ],
                    )?;
                    canvas.present();
                } else if disconnect.fade.tick(MODAL_FADE) {
                    // The close-fade (Cancel/Back, or a confirmed Disconnect) just
                    // finished this tick. A confirmed Disconnect breaks out of the
                    // loop now — the fade already played, and the pre-stream UI takes
                    // the canvas over next, so there's nothing to wipe for that case.
                    if let Some(outcome) = pending_outcome.take() {
                        break 'running outcome;
                    }
                    // Otherwise (Cancel/Back): wipe the last frame so it doesn't stick
                    // over the video, same as a stats-overlay toggle-off.
                    canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                    canvas.clear();
                    canvas.present();
                    canvas.clear();
                    canvas.present();
                }
                // Offloaded audio is drained by its own dedicated thread instead (see
                // `session::ndl_audio_pump`) — nothing to do here.
                if let Some(player) = &mut audio_player {
                    session::pump_audio_once(&connected.client, player);
                }
                // Skipped whenever the dialog block drew this tick (open or still
                // fading out) — its own redraw above already owns the canvas. Stats
                // and the log overlay share one clear/execute/present: each does its
                // own `canvas.clear()` otherwise, which would erase whichever tile the
                // other just drew.
                let log_lines = log_overlay_lines();
                if (stats_enabled || log_lines.is_some())
                    && dialog_frame.is_none()
                    && overlay_last.is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
                {
                    overlay_last = Some(Instant::now());
                    let mut cmds: Vec<DrawCmd> = Vec::new();
                    if stats_enabled {
                        let frames = connected.stats.frames.load(Ordering::Relaxed);
                        let bytes = connected.stats.bytes.load(Ordering::Relaxed);
                        let dt = overlay_prev_at.elapsed().as_secs_f32().max(0.001);
                        let fps = (frames.saturating_sub(overlay_prev_frames)) as f32 / dt;
                        // Measured (received bytes/dt), vs. `resolved_bitrate_kbps` (negotiated).
                        let actual_kbps = (bytes.saturating_sub(overlay_prev_bytes)) as f32 * 8.0 / 1000.0 / dt;
                        overlay_prev_frames = frames;
                        overlay_prev_bytes = bytes;
                        overlay_prev_at = Instant::now();
                        let mode = connected.client.mode();
                        let feed_ms = connected.stats.feed_us.load(Ordering::Relaxed) as f32 / 1000.0;
                        let holding = connected.stats.holding.load(Ordering::Relaxed);
                        // CPU% (of one core) + RSS; only read while the overlay is up.
                        let cpu_mem_line = session::process_cpu_mem().map(|(cpu_ticks, mem_bytes)| {
                            // No baseline on the first sample, so CPU shows only from the 2nd on.
                            let cpu = overlay_prev_cpu_ticks.map(|prev| {
                                let pct = (cpu_ticks.saturating_sub(prev)) as f32
                                    / session::clock_ticks_per_sec() as f32
                                    / dt
                                    * 100.0;
                                format!("CPU {pct:.0}% · ")
                            });
                            overlay_prev_cpu_ticks = Some(cpu_ticks);
                            format!(
                                "{}RAM {:.0} MB",
                                cpu.unwrap_or_default(),
                                mem_bytes as f32 / (1024.0 * 1024.0)
                            )
                        });
                        let mut lines = vec![
                            format!(
                                "{}x{}@{} {}{} · {}",
                                mode.width,
                                mode.height,
                                mode.refresh_hz,
                                session::codec_name(connected.client.codec),
                                if connected.client.color.is_hdr() { " HDR" } else { "" },
                                connected.backend_name,
                            ),
                            format!("Video {fps:.1} fps · {frames} frames"),
                            {
                                // `backlog` is NDL's own undecoded/unpresented depth: rising
                                // means the decoder is behind, flat-near-zero while the
                                // picture stutters means the problem is upstream of it.
                                let backlog = connected.stats.render_backlog.load(Ordering::Relaxed);
                                let backlog = if backlog < 0 {
                                    "n/a".to_string()
                                } else {
                                    backlog.to_string()
                                };
                                format!(
                                    "Drop {} · FEC {} · hold {} · buf {backlog}",
                                    connected.client.frames_dropped(),
                                    connected.client.fec_recovered_shards(),
                                    if holding { "yes" } else { "no" },
                                )
                            },
                            format!(
                                "Feed {feed_ms:.1} ms · {:.0}/{} Mbps",
                                actual_kbps / 1000.0,
                                connected.client.resolved_bitrate_kbps / 1000,
                            ),
                        ];
                        if let Some(line) = cpu_mem_line {
                            lines.push(line);
                        }
                        match crate::ui::render_stats_overlay_tile(
                            fonts.value,
                            fonts.caption,
                            &lines,
                            "Press green button to hide this overlay",
                        ) {
                            Ok(tile) => {
                                let (tw, th) = (tile.width(), tile.height());
                                compositor.upload(&texture_creator, Tile::StatsOverlay, &tile)?;
                                cmds.push(DrawCmd::Tex {
                                    tile: Tile::StatsOverlay,
                                    dst: sdl2::rect::Rect::new(display_mode.w - tw as i32 - 24, 24, tw, th),
                                    alpha: 0xff,
                                });
                            }
                            Err(e) => tracing::warn!("stats overlay render failed: {e:#}"),
                        }
                    }
                    if let Some(lines) = log_lines {
                        match crate::ui::render_log_overlay_tile(fonts.caption, display_mode.w as u32, &lines) {
                            Ok(tile) => {
                                let (tw, th) = (tile.width(), tile.height());
                                compositor.upload(&texture_creator, Tile::LogOverlay, &tile)?;
                                cmds.push(DrawCmd::Tex {
                                    tile: Tile::LogOverlay,
                                    dst: sdl2::rect::Rect::new(0, display_mode.h - th as i32, tw, th),
                                    alpha: 0xff,
                                });
                            }
                            Err(e) => tracing::warn!("log overlay render failed: {e:#}"),
                        }
                    }
                    if !cmds.is_empty() {
                        canvas.set_blend_mode(sdl2::render::BlendMode::None);
                        canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                        canvas.clear();
                        compositor.execute(&mut canvas, &cmds)?;
                        canvas.present();
                    }
                }
                if connected.client.is_session_ended() {
                    tracing::info!("host ended the session");
                    break 'running StreamOutcome::ReturnToMenu;
                }

                // This tick bounds how stale a forwarded input event or a queued audio
                // packet can get (video has its own thread and never waits on this loop).
                // 8ms here meant up to 8ms added to every remote/gamepad event and to the
                // audio drain cadence; at 2ms an idle poll_iter + try-recv round is a few
                // microseconds of work, so ~500 wakeups/s is noise even on this SoC while
                // keeping the added input latency near zero.
                std::thread::sleep(Duration::from_millis(2));
            };

            // `disconnect_quit()` was already called above for every deliberate-stop path;
            // `shutdown()` joins the video thread and drops `client` so the QUIC close
            // frame actually gets sent before this function returns (see its docs).
            connected.shutdown();
            crate::ndl::quit();
            sdl.mouse().show_cursor(true);
            match outcome {
                StreamOutcome::Quit => {
                    tracing::info!("punktfunk-webos exiting cleanly");
                    return Ok(());
                }
                StreamOutcome::ReturnToMenu => continue,
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod real {
    pub fn run() -> anyhow::Result<()> {
        anyhow::bail!(
            "punktfunk-webos only runs under target_os = \"linux\" (a native Linux box, \
             or the armv7-unknown-linux-gnueabi webOS cross target) — see Cargo.toml"
        );
    }
}

fn main() -> anyhow::Result<()> {
    real::run()
}
