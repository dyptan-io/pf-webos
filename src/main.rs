//! Native webOS TV client for punktfunk. See `docs/NOTES.md` for the architecture and
//! the hard-won platform gotchas. Real body only under `target_os = "linux"` (true
//! both on a native Linux dev box and the webOS `armv7-unknown-linux-gnueabi` cross
//! target, which reports the same `target_os`) — this keeps `cargo build` green on
//! macOS/Windows dev boxes without SDL2 installed.
#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod art;
#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod discovery;
#[cfg(target_os = "linux")]
mod gamepad;
#[cfg(target_os = "linux")]
mod keyboard;
#[cfg(target_os = "linux")]
mod library;
#[cfg(target_os = "linux")]
mod mouse;
#[cfg(target_os = "linux")]
mod ndl;
#[cfg(target_os = "linux")]
mod session;
#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
mod ui;
#[cfg(target_os = "linux")]
mod wol;

#[cfg(target_os = "linux")]
mod real {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use punktfunk_core::config::Mode;
    use sdl2::controller::GameController;
    use sdl2::mouse::MouseButton;

    use crate::app::{App, Screen};
    use crate::gamepad;
    use crate::keyboard;
    use crate::mouse;
    use crate::session;
    use crate::store;
    use crate::ui::MenuEvent;

    /// What `run_ui_flow` resolved: host, port, the pinned fingerprint (`None` for a
    /// fresh TOFU connect), and an optional library entry id to launch into.
    type ConnectOutcome = (String, u16, Option<[u8; 32]>, Option<String>);

    /// Set by [`handle_term_signal`], read by both event loops below as an extra
    /// "should we quit" condition alongside SDL's own `Event::Quit`. webOS can ask a
    /// backgrounded/closing app to exit via SIGTERM before ever reaching SIGKILL —
    /// without catching that, a stream in progress gets killed with no chance to
    /// tell the host anything (see `session::Connected::shutdown`'s docs).
    static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

    /// Async-signal-safe by construction (a lone atomic store) — real cleanup
    /// happens later, wherever `QUIT_REQUESTED` is next polled.
    extern "C" fn handle_term_signal(_signum: libc::c_int) {
        QUIT_REQUESTED.store(true, Ordering::Relaxed);
    }

    /// `SIGTERM` (webOS's/systemd's normal "please exit") and `SIGINT` (Ctrl-C, for
    /// off-device smoke-testing). Best-effort: a failure just leaves the OS default
    /// (immediate kill) in place.
    fn install_signal_handlers() {
        // SAFETY: `libc::signal` with a function pointer of the correct
        // `extern "C" fn(c_int)` signature and no other arguments is exactly its
        // documented safe-to-call shape.
        unsafe {
            libc::signal(libc::SIGTERM, handle_term_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, handle_term_signal as *const () as libc::sighandler_t);
        }
    }

    /// webOS native apps run with no attached terminal; `dev-manager-desktop`'s log
    /// viewer reads this file. `/media/developer/apps/usr/palm/applications/<appid>/`
    /// is the app's own writable directory (falls back to `/tmp` off-device, e.g. when
    /// smoke-testing this binary on a Linux dev box before packaging).
    fn log_path() -> PathBuf {
        std::env::var_os("PUNKTFUNK_WEBOS_LOG_DIR")
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
            .join("punktfunk-webos.log")
    }

    pub fn run() -> Result<()> {
        install_signal_handlers();
        let log_path = log_path();
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open log file {}", log_path.display()))?;
        writeln!(log, "punktfunk-webos starting")?;

        // Without this, punktfunk-core's own `tracing::info!`/`warn!` calls — including the
        // startup link-capacity probe's measured throughput and the ABR ceiling it derives from
        // it — are silent no-ops (nothing installs a subscriber by default). A fresh handle to
        // the same file, not `log.try_clone()`, since this subscriber outlives `run_inner`'s
        // `&mut log` borrow for the whole process.
        let tracing_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open log file {} for tracing", log_path.display()))?;
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(tracing_log))
            .with_ansi(false)
            .with_target(false)
            .init();

        // Errors from here on only ever reached stderr, which is invisible for a
        // webOS native app with no attached terminal.
        match run_inner(&mut log) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = writeln!(log, "error: {e:#}");
                Err(e)
            }
        }
    }

    /// How long the keyboard/gamepad Back-equivalent must be held during a
    /// stream before it disconnects and returns to the menu — long enough that a
    /// normal game-input tap of the same physical button (many games use
    /// B/Back-ish buttons) never triggers it.
    const LONG_PRESS_BACK: Duration = Duration::from_millis(1500);
    /// How long the Magic Remote's OK button (the pointer's left click) must be held
    /// before it's promoted to a right click — long enough that a normal click never
    /// trips it, short enough to feel deliberate rather than sluggish.
    const LONG_PRESS_OK: Duration = Duration::from_millis(500);

    /// How long OK must be held on a sidebar host row before it opens
    /// `Screen::ForgetHost` instead of that row's normal short-press action
    /// (connect/pair) — see `run_ui_flow`'s `confirm_held_since`. Short enough
    /// not to feel unresponsive, long enough that a normal tap never triggers it.
    const LONG_PRESS_CONFIRM: Duration = Duration::from_millis(500);

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

    /// Starts/clears the streaming loop's Back long-press timer (see
    /// `LONG_PRESS_BACK`) — shared by the keyboard and controller arms, which
    /// otherwise repeat this identically.
    fn track_back_hold(is_back: bool, pressed: bool, held_since: &mut Option<Instant>) {
        if !is_back {
            return;
        }
        if pressed {
            held_since.get_or_insert_with(Instant::now);
        } else {
            *held_since = None;
        }
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
        frame_texture: &mut sdl2::render::Texture,
        painter: &mut crate::ui::Painter,
        events: &mut sdl2::EventPump,
        game_controller: &sdl2::GameControllerSubsystem,
        controller: &mut Option<GameController>,
        identity: &(String, String),
        display_mode: sdl2::video::DisplayMode,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
        icon_font: &sdl2::ttf::Font,
        log: &mut std::fs::File,
    ) -> Result<Option<ConnectOutcome>> {
        // Test/dev override: skip the UI entirely if a connect.conf was dropped
        // alongside sideloading (see store.rs docs) — the UI flow is the normal path.
        // Bypasses the library screen too (`launch: None`, a plain desktop session).
        if let Some((host, port)) = store::dev_override_connect() {
            writeln!(log, "dev override: connecting to {host}:{port}")?;
            return Ok(Some((host, port, None, None)));
        }

        canvas.window_mut().show();
        let mut app = App::new(identity.clone(), log);
        // Rasterized-text cache (see `ui::TextCache` docs) — created once here and
        // threaded down through every render call for the rest of this UI-flow's
        // lifetime so repeat draws of the same (font, text, color) reuse an
        // already-rasterized+premultiplied `Pixmap` instead of re-rasterizing
        // freetype glyphs on every ~60fps tick.
        let mut text_cache = crate::ui::TextCache::new();
        // Tracks an in-progress OK hold on a sidebar host row — see
        // `LONG_PRESS_CONFIRM`'s docs and the poll below. `None` whenever OK
        // isn't currently down (or the down happened somewhere a hold has no
        // special meaning, e.g. a grid card, so it was dispatched immediately
        // instead of being intercepted here at all).
        let mut confirm_held_since: Option<Instant> = None;
        // Set once the hold has already crossed `LONG_PRESS_CONFIRM` and
        // opened `Screen::ForgetHost`, so the matching key-up doesn't *also*
        // fire that row's normal short-press action.
        let mut confirm_long_fired = false;
        // Whether a Back-mapped key/button is currently held, per the
        // keyboard/gamepad event stream — edge-detected so a single physical
        // press dispatches Back exactly once no matter how SDL reports (or
        // misreports) repeats for it.
        let mut menu_back_down = false;
        let mut stick_nav = crate::ui::StickMenuNav::default();
        // Redraw-on-change: this screen has no time-based animation at all (no
        // spinner/blink/marquee), so every pixel that can change only ever changes
        // as a reaction to one of: an SDL event or a Discovery/art/library
        // background result — anything else is a no-op tick. Without this,
        // `app.render(...)` (and the `canvas.present()` vsync swap inside it) ran
        // unconditionally every 16ms forever, even sitting on an untouched menu.
        // Starts `true` so the first frame always draws.
        let mut dirty = true;
        let target = 'ui: loop {
            if QUIT_REQUESTED.load(Ordering::Relaxed) {
                writeln!(log, "SIGTERM/SIGINT received during UI")?;
                return Ok(None);
            }
            dirty |= app.drain_discovery(log);
            dirty |= app.drain_art();
            dirty |= app.drain_games(log);
            dirty |= app.tick_wake(log);
            dirty |= app.drain_launch_check(log);
            if let Some(target) = app.take_ready_launch() {
                break 'ui target;
            }
            for event in events.poll_iter() {
                use sdl2::event::Event;
                if let Event::Quit { .. } = event {
                    writeln!(log, "quit during UI")?;
                    return Ok(None);
                }
                // The Magic Remote's pointer mode surfaces as a plain SDL2
                // MouseMotion event fired continuously while the remote is
                // moving — unlike every other event handled below, redraw only
                // if the motion actually changed the focused/hovered element,
                // not on every no-op tick.
                if let Event::MouseMotion { x, y, .. } = event {
                    dirty |= app.handle_mouse_motion(x, y, display_mode.w as u32, display_mode.h as u32);
                    continue;
                }
                // Any other event might change what's on screen (focus/hover, a typed
                // digit, a screen transition) — simplest to mark dirty for all of
                // them rather than re-litigate that per event kind.
                dirty = true;
                match event {
                    // Same hold-vs-tap split as the keyboard/gamepad arms below,
                    // for the Magic Remote's pointer mode: it delivers OK as a
                    // plain mouse click, not a key event, so a physical
                    // press-and-hold here never reached the keyboard/gamepad
                    // arms at all — hit-test+focus the press's own position
                    // fresh (see `App::focus_host_row_at`'s docs on why), then
                    // only hold-time it while actually landing on a host row.
                    Event::MouseButtonDown {
                        mouse_btn: sdl2::mouse::MouseButton::Left,
                        x,
                        y,
                        ..
                    } => {
                        if app.focus_host_row_at(x, y, display_mode.h as u32).is_some() {
                            confirm_held_since.get_or_insert_with(Instant::now);
                            continue;
                        }
                        if let Some(target) =
                            app.handle_mouse_click(x, y, display_mode.w as u32, display_mode.h as u32, log)
                        {
                            break 'ui target;
                        }
                        continue;
                    }
                    Event::MouseButtonUp {
                        mouse_btn: sdl2::mouse::MouseButton::Left,
                        x,
                        y,
                        ..
                    } => {
                        let held = confirm_held_since.take().is_some();
                        if held && !confirm_long_fired {
                            if let Some(target) =
                                app.handle_mouse_click(x, y, display_mode.w as u32, display_mode.h as u32, log)
                            {
                                break 'ui target;
                            }
                        }
                        confirm_long_fired = false;
                        continue;
                    }
                    // OK pressed down on a sidebar host row: don't dispatch its
                    // normal short-press action (connect/pair) yet — start
                    // timing the hold instead, so a long-enough press opens
                    // `Screen::ForgetHost` (checked below) instead.
                    // `get_or_insert_with` rather than an unconditional
                    // `Some(Instant::now())`: a held key can resend `KeyDown`
                    // as an OS repeat, which must not keep resetting the clock
                    // back to zero.
                    //
                    // `confirm_held_since.is_some()` alone (not just
                    // `host_row_focused()`) also keeps intercepting *once a hold is
                    // already tracked* — needed because the moment the hold crosses
                    // `LONG_PRESS_CONFIRM` and opens `Screen::ForgetHost`, the screen
                    // is no longer `Home`, so `host_row_focused()` goes back to
                    // `None` while the physical key is still down. Without this, the
                    // very next OS repeat `KeyDown` for that same still-held key fell
                    // through all the way to the generic dispatch below, which read
                    // it as a fresh Confirm on `Screen::ForgetHost` — landing on
                    // "Cancel" (the default focus) and dismissing the just-opened
                    // dialog before the user ever released the button. A held mouse
                    // button has no equivalent repeat (`MouseButtonDown` fires once),
                    // which is why this only ever showed up with keyboard/remote.
                    Event::KeyDown { keycode: Some(k), .. }
                        if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Confirm)
                            && (confirm_held_since.is_some() || app.host_row_focused().is_some()) =>
                    {
                        confirm_held_since.get_or_insert_with(Instant::now);
                        continue;
                    }
                    Event::ControllerButtonDown { button, .. }
                        if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Confirm)
                            && (confirm_held_since.is_some() || app.host_row_focused().is_some()) =>
                    {
                        confirm_held_since.get_or_insert_with(Instant::now);
                        continue;
                    }
                    // Released before crossing `LONG_PRESS_CONFIRM`: it was a
                    // plain tap, so fire the short-press action now, on
                    // release, instead of on the down this intercepted.
                    // Already-long-fired (menu opened) or never intercepted in
                    // the first place (`confirm_held_since` is `None`): do
                    // nothing here, the normal dispatch below already handled
                    // (or never needed to handle) it.
                    Event::KeyUp { keycode: Some(k), .. }
                        if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Confirm) =>
                    {
                        let held = confirm_held_since.take().is_some();
                        // Re-checks `Screen::Home` rather than trusting the
                        // hold was still valid: a background event (e.g. a
                        // library fetch failing into the Wake prompt) could in
                        // principle have changed screens while OK was down.
                        if held && !confirm_long_fired && matches!(app.screen, Screen::Home) {
                            if let Some(target) = app.handle_home_event(MenuEvent::Confirm, display_mode.w as u32, log)
                            {
                                break 'ui target;
                            }
                        }
                        confirm_long_fired = false;
                        continue;
                    }
                    Event::ControllerButtonUp { button, .. }
                        if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Confirm) =>
                    {
                        let held = confirm_held_since.take().is_some();
                        if held && !confirm_long_fired && matches!(app.screen, Screen::Home) {
                            if let Some(target) = app.handle_home_event(MenuEvent::Confirm, display_mode.w as u32, log)
                            {
                                break 'ui target;
                            }
                        }
                        confirm_long_fired = false;
                        continue;
                    }
                    // Direct digit entry via the remote's number buttons — PIN entry
                    // on the pairing screen, IP:port entry on the add-host screen.
                    Event::KeyDown { keycode: Some(k), .. }
                        if matches!(app.screen, Screen::Pairing | Screen::AddHost) =>
                    {
                        if let Some(digit) = crate::ui::digit_key_value(k) {
                            match app.screen {
                                Screen::Pairing => app.enter_pin_digit(digit, log),
                                Screen::AddHost => app.enter_add_host_digit(digit),
                                _ => unreachable!(),
                            }
                            continue;
                        }
                    }
                    _ => {}
                }
                let menu_ev = match event {
                    Event::KeyDown { keycode: Some(k), .. } => {
                        edge_trigger_back(crate::ui::menu_event_for_key(k), &mut menu_back_down)
                    }
                    Event::KeyUp { keycode: Some(k), .. } => {
                        if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back) {
                            menu_back_down = false;
                        }
                        None
                    }
                    Event::ControllerButtonDown { button, .. } => {
                        edge_trigger_back(crate::ui::menu_event_for_button(button), &mut menu_back_down)
                    }
                    Event::ControllerButtonUp { button, .. } => {
                        if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Back) {
                            menu_back_down = false;
                        }
                        None
                    }
                    Event::ControllerDeviceAdded { which, .. } if controller.is_none() => {
                        match game_controller.open(which) {
                            Ok(c) => {
                                writeln!(log, "controller connected: {}", c.name())?;
                                *controller = Some(c);
                            }
                            Err(e) => writeln!(log, "controller open failed: {e}")?,
                        }
                        None
                    }
                    Event::ControllerDeviceRemoved { .. } => {
                        *controller = None;
                        None
                    }
                    Event::ControllerAxisMotion { axis, value, .. } => stick_nav.axis_event(axis, value),
                    _ => None,
                };
                let Some(menu_ev) = menu_ev else { continue };
                match app.screen {
                    // A keyboard/gamepad Back is a bonus shortcut to Settings; the
                    // sidebar's own Settings row (reachable via Up/Down + Confirm)
                    // is the reliable primary path. `App::back` (shared with the
                    // close-button click path) is what actually decides that.
                    Screen::Home => {
                        if menu_ev == MenuEvent::Back {
                            if let Some(target) = app.back(log) {
                                break 'ui target;
                            }
                        } else if let Some(target) = app.handle_home_event(menu_ev, display_mode.w as u32, log) {
                            break 'ui target;
                        }
                    }
                    Screen::Pairing => app.handle_pairing_event(menu_ev, log),
                    Screen::Settings => app.handle_settings_event(menu_ev),
                    Screen::AddHost => app.handle_add_host_event(menu_ev),
                    Screen::Wake => app.handle_wake_event(menu_ev, log),
                    Screen::ForgetHost => app.handle_forget_host_event(menu_ev),
                }
            }
            // Confirm hold-to-open-the-Forget-confirmation threshold (see
            // `LONG_PRESS_CONFIRM`'s docs) — checked live each tick rather than
            // waiting for release, so it appears the instant the hold is long
            // enough instead of only once OK comes back up.
            if !confirm_long_fired {
                if let Some(since) = confirm_held_since {
                    if since.elapsed() >= LONG_PRESS_CONFIRM {
                        if let Some(idx) = app.host_row_focused() {
                            app.open_forget_host(idx);
                            dirty = true;
                        }
                        confirm_long_fired = true;
                    }
                }
            }
            if !dirty {
                std::thread::sleep(Duration::from_millis(16));
                continue;
            }
            dirty = false;
            app.render(
                painter,
                &mut text_cache,
                font_label,
                font_value,
                font_title,
                icon_font,
                display_mode.w as u32,
                display_mode.h as u32,
            )?;
            frame_texture
                .update(None, painter.data(), (display_mode.w as usize) * 4)
                .map_err(|e| anyhow::anyhow!("update frame texture: {e}"))?;
            canvas
                .copy(frame_texture, None, None)
                .map_err(|e| anyhow::anyhow!("copy frame texture: {e}"))?;
            canvas.present();
            std::thread::sleep(Duration::from_millis(16));
        };
        Ok(Some((
            target.host,
            target.port,
            Some(target.fingerprint),
            target.launch,
        )))
    }

    fn run_inner(log: &mut std::fs::File) -> Result<()> {
        // Without this, webOS's system launcher intercepts the Magic Remote's Back
        // key before the app ever sees it (confirmed on-device — see
        // docs/NOTES.md's "Tried and abandoned" note on this exact hint, previously
        // removed after it appeared not to help; re-trying it here). Must be set
        // before the window is created — `SDL_waylandwebos.c` only applies it at
        // window-creation time (plus a runtime hint-watcher for later changes).
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "true");
        // SDL2 disables "extended" HID reports for PS4/PS5 pads over Bluetooth by
        // default — and rumble is only carried in the extended report, so a
        // Bluetooth DualSense/DualShock4 otherwise never vibrates no matter what
        // `GameController::set_rumble` is called with. Must be set before the game
        // controller subsystem opens the pad (SDL reads these at HIDAPI driver init).
        sdl2::hint::set("SDL_JOYSTICK_HIDAPI_PS5_RUMBLE", "1");
        sdl2::hint::set("SDL_JOYSTICK_HIDAPI_PS4_RUMBLE", "1");
        let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL_Init: {e}"))?;
        let ttf = sdl2::ttf::init().map_err(|e| anyhow::anyhow!("SDL_ttf init: {e}"))?;
        let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video subsystem: {e}"))?;
        let game_controller = sdl
            .game_controller()
            .map_err(|e| anyhow::anyhow!("SDL game controller subsystem: {e}"))?;
        let sdl_audio = sdl.audio().map_err(|e| anyhow::anyhow!("SDL audio subsystem: {e}"))?;
        writeln!(log, "SDL video subsystem up (driver: {})", video.current_video_driver())?;

        let display_mode = video
            .current_display_mode(0)
            .map_err(|e| anyhow::anyhow!("current_display_mode: {e}"))?;
        writeln!(
            log,
            "display mode: {}x{}@{}",
            display_mode.w, display_mode.h, display_mode.refresh_rate
        )?;

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
        writeln!(log, "window + canvas created")?;

        // The pre-stream UI's whole rendering backend (see `ui.rs`'s module docs):
        // `App::render` draws every screen into this one `Painter` (a software
        // framebuffer) each dirty tick; `run_ui_flow` then uploads the finished
        // buffer here and presents it — one texture/copy per frame, not one per
        // widget/art-cover/text-label the way the old canvas-primitives version did.
        let mut painter = crate::ui::Painter::new(display_mode.w as u32, display_mode.h as u32);
        // STREAMING (not STATIC) — this texture's whole content is re-uploaded via
        // `update()` every dirty tick, which is exactly the frequent-full-update
        // case STREAMING is meant for; STATIC targets content that rarely changes
        // and can be a slower path for a per-frame full-frame upload on some
        // backends.
        let mut frame_texture = texture_creator
            .create_texture_streaming(
                sdl2::pixels::PixelFormatEnum::RGBA32,
                display_mode.w as u32,
                display_mode.h as u32,
            )
            .map_err(|e| anyhow::anyhow!("create frame texture: {e}"))?;

        let mut events = sdl.event_pump().map_err(|e| anyhow::anyhow!("event pump: {e}"))?;

        let identity = store::load_or_create_identity().context("load_or_create_identity")?;

        // Sized for a 10-foot TV viewing distance — see ui.rs's ROW_H/ROW_MAX_W docs.
        let font_label = crate::ui::load_font(&ttf, display_mode.h as u32, 22)?;
        let font_value = crate::ui::load_font(&ttf, display_mode.h as u32, 20)?;
        let font_title = crate::ui::load_font(&ttf, display_mode.h as u32, 40)?;
        let icon_font = crate::ui::load_icon_font(&ttf)?;

        // Owned here, at the top of the menu/stream cycle, rather than re-declared in
        // each: `ControllerDeviceAdded` only fires once per physical (re)connection, so
        // a pad already open from the menu (or a previous stream) needs to carry
        // straight through a screen transition instead of waiting for a replug neither
        // side will ever see.
        let mut controller: Option<GameController> = None;

        loop {
            let Some((host, port, fp, launch)) = run_ui_flow(
                &mut canvas,
                &mut frame_texture,
                &mut painter,
                &mut events,
                &game_controller,
                &mut controller,
                &identity,
                display_mode,
                &font_label,
                &font_value,
                &font_title,
                &icon_font,
                log,
            )?
            else {
                writeln!(log, "punktfunk-webos exiting cleanly")?;
                return Ok(());
            };

            let settings = store::load_settings();
            writeln!(log, "settings: {settings:?}")?;

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
            // The system draws its own cursor (a real SDL2 cursor this fork loads from
            // `/usr/share/im/...` — confirmed via `SDL_waylandwebos_cursor.c`) tracking
            // the physical remote directly; the host draws a second, independent one
            // wherever our forwarded `MouseMoveAbs` puts it. Two visible cursors reads
            // as "the pointer doesn't match the remote" — hide the local one so only
            // the host's shows. Restored when back in the menu (`sdl.mouse()` is the
            // same standard SDL2 API on any platform, not webOS-specific).
            sdl.mouse().show_cursor(false);

            // SDL2/Wayland reports refresh_rate=0 in some launch contexts (confirmed:
            // the host's virtual-display driver rejected a literal "0 Hz" mode request
            // with "the parameter is incorrect") — the settings' own nominal rate (never
            // 0; see store::Settings::default) is what drives the wire value directly.
            writeln!(
                log,
                "requesting {}x{}@{}",
                settings.width, settings.height, settings.refresh_hz
            )?;
            let mode = Mode {
                width: settings.width,
                height: settings.height,
                refresh_hz: settings.refresh_hz,
            };
            let connected = session::connect(
                &host,
                port,
                mode,
                settings.bitrate_kbps,
                settings.hdr_enabled,
                identity.clone(),
                fp,
                launch,
                // The host PARKS an unpinned/TOFU connect until an operator approves it —
                // matching clients/session's PENDING_APPROVAL_WAIT convention, not the
                // plain 15s handshake budget (too short for a human to notice and click).
                Duration::from_secs(185),
                display_mode.w,
                display_mode.h,
                log,
            )
            .context("session connect")?;
            writeln!(log, "session connected, entering event loop")?;

            let mut audio_player = crate::audio::AudioPlayer::new(&sdl_audio, connected.client.audio_channels)
                .context("audio player init")?;
            writeln!(
                log,
                "SDL audio driver: {}, spec: {:?}",
                sdl_audio.current_audio_driver(),
                audio_player.spec()
            )?;

            let mut back_held_since: Option<Instant> = None;
            let mut ok_held_since: Option<Instant> = None;
            let mut ok_promoted = false;
            let mut scroll_acc = mouse::ScrollAccumulator::default();
            let outcome = 'running: loop {
                if QUIT_REQUESTED.load(Ordering::Relaxed) {
                    writeln!(log, "SIGTERM/SIGINT received — disconnecting before exit")?;
                    connected.client.disconnect_quit();
                    break 'running StreamOutcome::Quit;
                }
                for event in events.poll_iter() {
                    use sdl2::event::Event;
                    match event {
                        Event::Quit { .. } => {
                            // As deliberate a stop as long-press-Back below — tear the
                            // virtual display down now instead of lingering for a reconnect.
                            connected.client.disconnect_quit();
                            break 'running StreamOutcome::Quit;
                        }
                        Event::ControllerDeviceAdded { which, .. } if controller.is_none() => {
                            match game_controller.open(which) {
                                Ok(c) => {
                                    writeln!(log, "controller connected: {}", c.name())?;
                                    controller = Some(c);
                                }
                                Err(e) => writeln!(log, "controller open failed: {e}")?,
                            }
                        }
                        Event::ControllerDeviceRemoved { .. } => {
                            controller = None;
                        }
                        Event::KeyDown { keycode: Some(k), .. } => {
                            let is_back = crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back);
                            track_back_hold(is_back, true, &mut back_held_since);
                            if let Some(ev) = keyboard::key_event(k, true) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        Event::KeyUp { keycode: Some(k), .. } => {
                            let is_back = crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back);
                            track_back_hold(is_back, false, &mut back_held_since);
                            if let Some(ev) = keyboard::key_event(k, false) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        Event::ControllerButtonDown { button, .. } => {
                            let is_back = crate::ui::menu_event_for_button(button) == Some(MenuEvent::Back);
                            track_back_hold(is_back, true, &mut back_held_since);
                            let ev = gamepad::button_event(button, true, 0);
                            let _ = session::send_input(&connected.client, &ev);
                        }
                        Event::ControllerButtonUp { button, .. } => {
                            let is_back = crate::ui::menu_event_for_button(button) == Some(MenuEvent::Back);
                            track_back_hold(is_back, false, &mut back_held_since);
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
                            if mouse_btn == MouseButton::Left {
                                ok_held_since = Some(Instant::now());
                                ok_promoted = false;
                            }
                            if let Some(ev) = mouse::button_event(mouse_btn, true) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        Event::MouseButtonUp { mouse_btn, .. } => {
                            let released = if mouse_btn == MouseButton::Left && ok_promoted {
                                MouseButton::Right
                            } else {
                                mouse_btn
                            };
                            if mouse_btn == MouseButton::Left {
                                ok_held_since = None;
                                ok_promoted = false;
                            }
                            if let Some(ev) = mouse::button_event(released, false) {
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
                if back_held_since.is_some_and(|t| t.elapsed() >= LONG_PRESS_BACK) {
                    writeln!(log, "back — disconnecting to menu")?;
                    // A deliberate user stop (not a network drop/backgrounding) — the
                    // host tears the virtual display down immediately instead of
                    // lingering for a reconnect that isn't coming (see the embedding
                    // guide's teardown section). Every other exit path (host ended
                    // the session, app quit) leaves this alone and just drops the
                    // client, which closes with the "may reconnect" code instead.
                    connected.client.disconnect_quit();
                    break 'running StreamOutcome::ReturnToMenu;
                }
                if !ok_promoted && ok_held_since.is_some_and(|t| t.elapsed() >= LONG_PRESS_OK) {
                    // Release left before pressing right so the host never sees both held.
                    if let Some(ev) = mouse::button_event(MouseButton::Left, false) {
                        let _ = session::send_input(&connected.client, &ev);
                    }
                    if let Some(ev) = mouse::button_event(MouseButton::Right, true) {
                        let _ = session::send_input(&connected.client, &ev);
                    }
                    ok_promoted = true;
                }
                session::pump_audio_once(&connected.client, &mut audio_player, log);
                session::pump_rumble_once(&connected.client, &mut controller, log);
                if connected.client.is_session_ended() {
                    writeln!(log, "host ended the session")?;
                    break 'running StreamOutcome::ReturnToMenu;
                }

                std::thread::sleep(Duration::from_millis(8));
            };

            // `disconnect_quit()` was already called above for every deliberate-stop path;
            // `shutdown()` joins the video thread and drops `client` so the QUIC close
            // frame actually gets sent before this function returns (see its docs).
            connected.shutdown();
            crate::ndl::quit();
            sdl.mouse().show_cursor(true);
            match outcome {
                StreamOutcome::Quit => {
                    writeln!(log, "punktfunk-webos exiting cleanly")?;
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
