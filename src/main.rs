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
mod compositor;
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
mod starfish;
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

    use crate::app::{App, Screen};
    use crate::compositor::{Compositor, DrawCmd, Tile};
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
        // Truncate (not append) — this file previously grew unbounded across every
        // launch for the life of the install; each run's log now starts fresh, and
        // `task deploy:log` tails it live anyway.
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
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
            dirty |= app.drain_pairing(log);
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
                    dirty |= app.handle_mouse_motion(
                        x,
                        y,
                        display_mode.w as u32,
                        display_mode.h as u32,
                        fonts,
                    );
                    continue;
                }
                // The Magic Remote's scroll wheel — scrolls the game grid on Home
                // (wheel y > 0 = "scroll up" = content moves down). Like motion
                // above, only redraws when the offset actually moved (a wheel tick
                // at either clamp edge is a no-op).
                if let Event::MouseWheel { y: wheel_y, .. } = event {
                    if matches!(app.screen, Screen::Home) {
                        /// Grid px scrolled per wheel detent — about a third of a
                        /// card row, so a few ticks walk one row.
                        const WHEEL_STEP: i32 = 120;
                        dirty |= app.scroll_grid_by(
                            -wheel_y * WHEEL_STEP,
                            display_mode.w as u32,
                            display_mode.h as u32,
                        );
                    }
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
                            app.handle_mouse_click(x, y, display_mode.w as u32, display_mode.h as u32, fonts, log)
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
                                app.handle_mouse_click(x, y, display_mode.w as u32, display_mode.h as u32, fonts, log)
                            {
                                break 'ui target;
                            }
                        }
                        confirm_long_fired = false;
                        continue;
                    }
                    // OK pressed down on a sidebar host row: start timing the hold
                    // instead of dispatching connect/pair immediately, so a
                    // long-enough press opens `Screen::ForgetHost` (checked below).
                    // `get_or_insert_with`, not `Some(Instant::now())`, since a held
                    // key resends `KeyDown` as an OS repeat and that must not keep
                    // resetting the clock. `confirm_held_since.is_some()` keeps
                    // intercepting those repeats even after `Screen::ForgetHost`
                    // opens (when `host_row_focused()` alone would go back to `None`)
                    // — otherwise the next repeat fell through to the generic
                    // dispatch below as a fresh Confirm and dismissed the dialog via
                    // its default-focused "Cancel" before the button was released.
                    // Mouse has no equivalent bug: `MouseButtonDown` never repeats.
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
                            if let Some(target) = app.handle_home_event(MenuEvent::Confirm, display_mode.w as u32, display_mode.h as u32, log)
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
                            if let Some(target) = app.handle_home_event(MenuEvent::Confirm, display_mode.w as u32, display_mode.h as u32, log)
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
                    // Back on Home is a no-op (root screen — `App::back` decides);
                    // routed through `back` anyway so the policy lives in one place.
                    Screen::Home => {
                        if menu_ev == MenuEvent::Back {
                            if let Some(target) = app.back(log) {
                                break 'ui target;
                            }
                        } else if let Some(target) = app.handle_home_event(menu_ev, display_mode.w as u32, display_mode.h as u32, log) {
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
            // Animations advance every 16ms tick and keep frames flowing on their
            // own; `dirty` (an event/drain changed actual content) additionally
            // forces stale tiles to re-rasterize.
            let animating = app.tick_animations();
            if !dirty && !animating {
                std::thread::sleep(Duration::from_millis(16));
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
            for tile in updated {
                if let Some(pm) = app.tile_pixmap(tile) {
                    compositor.upload(texture_creator, tile, pm)?;
                }
            }
            let cmds = app.draw_list(display_mode.w as u32, display_mode.h as u32, fonts);
            canvas.set_blend_mode(sdl2::render::BlendMode::None);
            canvas.set_draw_color(crate::ui::BG);
            canvas.clear();
            compositor.execute(canvas, &cmds)?;
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
        // Prevents webOS's system launcher from intercepting the Magic Remote's Back
        // key. Must be set before window creation.
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "true");
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
        writeln!(log, "window + canvas created (renderer: {})", canvas.info().name)?;

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
        let icon_font = crate::ui::load_icon_font(&ttf)?;
        let fonts = crate::ui::Fonts {
            label: &font_label,
            value: &font_value,
            title: &font_title,
            icon: &icon_font,
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
            let Some((host, port, fp, launch)) = run_ui_flow(
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
            let connected = match session::connect(
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
                settings.video_backend,
                log,
            ) {
                Ok(c) => c,
                Err(e) => {
                    // A failed connect (host went down in the race, codec/launch
                    // rejection, handshake error) used to `?` out of `run_inner`
                    // and take the whole app down — return to the menu with the
                    // reason on screen instead.
                    writeln!(log, "session connect failed: {e:#}")?;
                    sdl.mouse().show_cursor(true);
                    menu_status = Some(format!("Couldn't connect: {e:#}"));
                    continue;
                }
            };
            writeln!(log, "session connected, entering event loop")?;

            let mut audio_player = match crate::audio::AudioPlayer::new(&sdl_audio, connected.client.audio_channels)
            {
                Ok(p) => p,
                Err(e) => {
                    // Same no-crash policy as the connect above — including the
                    // video-side teardown the normal stream exit does, since the
                    // connect succeeded and loaded a decoder.
                    writeln!(log, "audio player init failed: {e:#}")?;
                    connected.client.disconnect_quit();
                    connected.shutdown();
                    crate::ndl::quit();
                    sdl.mouse().show_cursor(true);
                    menu_status = Some(format!("Couldn't start audio: {e:#}"));
                    continue;
                }
            };
            writeln!(
                log,
                "SDL audio driver: {}, spec: {:?}",
                sdl_audio.current_audio_driver(),
                audio_player.spec()
            )?;

            let mut scroll_acc = mouse::ScrollAccumulator::default();
            // In-stream stats overlay (Settings toggle): refreshed at ~2Hz onto the
            // otherwise-transparent stream window. Drawing composites OVER the
            // punch-through video plane via the surface's per-pixel alpha — the same
            // mechanism that lets the video show through the transparent clear. The
            // window is never shown/hidden here (that's what crashed the old overlay
            // attempt — see docs/NOTES.md).
            let stats_enabled = settings.stats_overlay;
            let mut overlay_last: Option<Instant> = None;
            let mut overlay_prev_frames: u64 = 0;
            let mut overlay_prev_at = Instant::now();
            // None = dialog not shown; Some(0) = shown, "Disconnect" focused;
            // Some(1) = shown, "Cancel" focused (default on open — safer).
            let mut disconnect_dialog: Option<usize> = None;
            // The shell (card/title/both buttons unfocused) only needs
            // re-rendering when the dialog opens; the focused button is its
            // own small tile (same shell/focus-tile split as every pre-stream
            // modal) so toggling focus never re-rasterizes the shell.
            let mut disconnect_shell_dirty = false;
            let mut disconnect_focus_dirty = false;
            let mut disconnect_focus_anim: Option<Instant> = None;
            let mut disconnect_tc = crate::ui::TextCache::new();
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
                        // Dialog open: navigate it only, don't forward input to the
                        // host. Non-repeat keys/fresh controller presses only, so the
                        // held Back that opened it doesn't also dismiss it.
                        _ if disconnect_dialog.is_some() => {
                            let focus = disconnect_dialog.expect("guarded by is_some above");
                            let nav = match &event {
                                Event::KeyDown { keycode: Some(k), repeat: false, .. } => {
                                    crate::ui::menu_event_for_key(*k)
                                }
                                Event::ControllerButtonDown { button, .. } => {
                                    crate::ui::menu_event_for_button(*button)
                                }
                                _ => None,
                            };
                            match nav {
                                Some(MenuEvent::Left) | Some(MenuEvent::Right) => {
                                    disconnect_dialog = Some(1 - focus);
                                    disconnect_focus_dirty = true;
                                    disconnect_focus_anim = Some(Instant::now());
                                }
                                Some(MenuEvent::Confirm) if focus == 0 => {
                                    writeln!(log, "back — disconnecting to menu")?;
                                    connected.client.disconnect_quit();
                                    break 'running StreamOutcome::ReturnToMenu;
                                }
                                Some(MenuEvent::Confirm) | Some(MenuEvent::Back) => {
                                    disconnect_dialog = None;
                                    // Clear both double-buffer frames back to transparent
                                    // (also wipes the stats overlay — force a redraw).
                                    canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                                    canvas.clear();
                                    canvas.present();
                                    canvas.clear();
                                    canvas.present();
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
                        Event::KeyDown { keycode: Some(k), scancode: None, repeat: false, .. }
                            if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back) =>
                        {
                            disconnect_dialog = Some(1);
                            disconnect_shell_dirty = true;
                            disconnect_focus_dirty = true;
                            disconnect_focus_anim = Some(Instant::now());
                        }
                        Event::KeyUp { scancode: Some(sc), .. } => {
                            if let Some(ev) = keyboard::key_event(sc, false) {
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        Event::ControllerButtonDown { button, .. } => {
                            if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Back) {
                                disconnect_dialog = Some(1);
                                disconnect_shell_dirty = true;
                                disconnect_focus_dirty = true;
                                disconnect_focus_anim = Some(Instant::now());
                            }
                            let ev = gamepad::button_event(button, true, 0);
                            let _ = session::send_input(&connected.client, &ev);
                        }
                        Event::ControllerButtonUp { button, .. } => {
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
                // Render the disconnect dialog when open. The card floats over
                // the live video (transparent surroundings); the shell
                // re-rasterizes only when the dialog opens, the focused
                // button only on focus change — but scrim + tiles recomposite
                // every tick (double buffered — a single present would leave
                // the other buffer stale), so the zoom-pop plays smoothly.
                if let Some(focus) = disconnect_dialog {
                    let full = sdl2::rect::Rect::new(0, 0, display_mode.w as u32, display_mode.h as u32);
                    if disconnect_shell_dirty {
                        disconnect_shell_dirty = false;
                        let shell = crate::ui::render_disconnect_dialog_shell(full.width(), full.height(), &fonts)?;
                        compositor.upload(&texture_creator, Tile::DisconnectDialog, &shell)?;
                    }
                    let (_, content) = crate::ui::disconnect_dialog_layout(full.width(), full.height(), fonts.label);
                    let btn_rect = crate::ui::confirm_button_rect(content, focus);
                    if disconnect_focus_dirty {
                        disconnect_focus_dirty = false;
                        let buttons = crate::ui::disconnect_dialog_buttons();
                        let tile = crate::ui::render_confirm_button_tile(
                            &mut disconnect_tc,
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
                    // for this.
                    let pad = crate::ui::ROW_TILE_PAD;
                    let base = sdl2::rect::Rect::new(
                        btn_rect.x() - pad,
                        btn_rect.y() - pad,
                        btn_rect.width() + 2 * pad as u32,
                        btn_rect.height() + 2 * pad as u32,
                    );
                    let f = crate::ui::anim_frac(disconnect_focus_anim, crate::ui::FOCUS_POP);
                    canvas.set_blend_mode(sdl2::render::BlendMode::None);
                    canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                    canvas.clear();
                    compositor.execute(
                        &mut canvas,
                        &[
                            DrawCmd::Fill { rect: full, color: sdl2::pixels::Color::RGBA(0, 0, 0, crate::ui::MODAL_SCRIM.a) },
                            DrawCmd::Tex { tile: Tile::DisconnectDialog, dst: full, alpha: 0xff },
                            DrawCmd::Tex {
                                tile: Tile::DisconnectFocusButton,
                                dst: crate::ui::zoom_rect(base, f, 0.02),
                                alpha: 0xff,
                            },
                        ],
                    )?;
                    canvas.present();
                }
                session::pump_audio_once(&connected.client, &mut audio_player, log);
                // Skipped while the dialog is open — its own redraw above already
                // owns the canvas this tick.
                if stats_enabled
                    && disconnect_dialog.is_none()
                    && overlay_last.is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
                {
                    overlay_last = Some(Instant::now());
                    let frames = connected.stats.frames.load(Ordering::Relaxed);
                    let dt = overlay_prev_at.elapsed().as_secs_f32().max(0.001);
                    let fps = (frames.saturating_sub(overlay_prev_frames)) as f32 / dt;
                    overlay_prev_frames = frames;
                    overlay_prev_at = Instant::now();
                    let mode = connected.client.mode();
                    let feed_ms =
                        connected.stats.feed_us.load(Ordering::Relaxed) as f32 / 1000.0;
                    let holding = connected.stats.holding.load(Ordering::Relaxed);
                    let lines = vec![
                        format!(
                            "{}x{}@{} {}{}",
                            mode.width,
                            mode.height,
                            mode.refresh_hz,
                            session::codec_name(connected.client.codec),
                            if connected.client.color.is_hdr() { " HDR" } else { "" },
                        ),
                        format!("Video {fps:.1} fps · {frames} frames"),
                        format!(
                            "Dropped {} · hold {}",
                            connected.client.frames_dropped(),
                            if holding { "yes" } else { "no" },
                        ),
                        format!(
                            "Feed {feed_ms:.1} ms · start {} Mbps",
                            connected.client.resolved_bitrate_kbps / 1000,
                        ),
                    ];
                    match crate::ui::render_stats_overlay_tile(fonts.value, &lines) {
                        Ok(tile) => {
                            let (tw, th) = (tile.width(), tile.height());
                            compositor.upload(&texture_creator, Tile::StatsOverlay, &tile)?;
                            canvas.set_blend_mode(sdl2::render::BlendMode::None);
                            canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                            canvas.clear();
                            compositor.execute(
                                &mut canvas,
                                &[DrawCmd::Tex {
                                    tile: Tile::StatsOverlay,
                                    dst: sdl2::rect::Rect::new(display_mode.w - tw as i32 - 24, 24, tw, th),
                                    alpha: 0xff,
                                }],
                            )?;
                            canvas.present();
                        }
                        Err(e) => writeln!(log, "stats overlay render failed: {e:#}")?,
                    }
                }
                if connected.client.is_session_ended() {
                    writeln!(log, "host ended the session")?;
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
