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
        let (writer, _guard) = crate::logger::init(&app_dir).context("init logger")?;
        tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_target(false)
            .with_max_level(crate::logger::resolved_level())
            .init();
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
        // Test/dev override: skip the UI entirely if a connect.conf was dropped
        // alongside sideloading (see store.rs docs) — the UI flow is the normal path.
        // Bypasses the library screen too (`launch: None`, a plain desktop session).
        if let Some((host, port)) = store::dev_override_connect() {
            tracing::info!("dev override: connecting to {host}:{port}");
            return Ok(Some((host, port, None, None)));
        }

        canvas.window_mut().show();
        let mut app = App::new(identity.clone());
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
        // Whether a Back-mapped key/button is currently held, per the
        // keyboard/gamepad event stream — edge-detected so a single physical
        // press dispatches Back exactly once no matter how SDL reports (or
        // misreports) repeats for it.
        let mut menu_back_down = false;
        let mut stick_nav = crate::ui::StickMenuNav::default();
        // Owned handle (it just clones the video subsystem's refcount), so taking it
        // here doesn't hold a borrow on `canvas` for the rest of the loop.
        let text_input = canvas.window().subsystem().text_input();
        tracing::info!(
            "on-screen keyboard support: {}",
            text_input.has_screen_keyboard_support()
        );
        let mut text_input_active = false;
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
                tracing::warn!("SIGTERM/SIGINT received during UI");
                return Ok(None);
            }
            dirty |= app.drain_discovery();
            dirty |= app.drain_art();
            dirty |= app.drain_games();
            dirty |= app.drain_pairing();
            dirty |= app.drain_speed_test();
            app.tick_reachability();
            dirty |= app.drain_reachability();
            dirty |= app.tick_wake();
            dirty |= app.drain_launch_check();
            if let Some(target) = app.take_ready_launch() {
                break 'ui target;
            }
            for event in events.poll_iter() {
                use sdl2::event::Event;
                if let Event::Quit { .. } = event {
                    tracing::info!("quit during UI");
                    return Ok(None);
                }
                // The Magic Remote's pointer mode surfaces as a plain SDL2
                // MouseMotion event fired continuously while the remote is
                // moving — unlike every other event handled below, redraw only
                // if the motion actually changed the focused/hovered element,
                // not on every no-op tick.
                if let Event::MouseMotion { x, y, .. } = event {
                    dirty |= app.handle_mouse_motion(x, y, display_mode.w as u32, display_mode.h as u32, fonts);
                    continue;
                }
                // The Magic Remote's scroll wheel — scrolls the game grid on Home
                // (wheel y > 0 = "scroll up" = content moves down). Like motion
                // above, only redraws when the offset actually moved (a wheel tick
                // at either clamp edge is a no-op).
                if let Event::MouseWheel { y: wheel_y, .. } = event {
                    if matches!(app.screen, Screen::About) {
                        /// Licence-wall px per wheel detent — a few lines at a time.
                        const ABOUT_WHEEL_STEP: i32 = 90;
                        dirty |= app.scroll_about_by(-wheel_y * ABOUT_WHEEL_STEP, fonts);
                        continue;
                    }
                    if matches!(app.screen, Screen::Home) {
                        /// Grid px scrolled per wheel detent — about a third of a
                        /// card row, so a few ticks walk one row.
                        const WHEEL_STEP: i32 = 120;
                        dirty |=
                            app.scroll_grid_by(-wheel_y * WHEEL_STEP, display_mode.w as u32, display_mode.h as u32);
                    }
                    continue;
                }
                // Any other event might change what's on screen (focus/hover, a typed
                // digit, a screen transition) — simplest to mark dirty for all of
                // them rather than re-litigate that per event kind.
                dirty = true;
                match event {
                    // The Magic Remote's pointer delivers OK as a plain mouse click.
                    // Dispatch it on press: there is no hold gesture to disambiguate
                    // any more (per-host actions have their own ⋯ button — see
                    // `ui::sidebar_menu_button_rect`), so nothing needs to wait for
                    // the release.
                    Event::MouseButtonDown {
                        mouse_btn: sdl2::mouse::MouseButton::Left,
                        x,
                        y,
                        ..
                    } => {
                        if let Some(target) =
                            app.handle_mouse_click(x, y, display_mode.w as u32, display_mode.h as u32, fonts)
                        {
                            break 'ui target;
                        }
                        continue;
                    }
                    // Direct digit entry via the remote's number buttons — PIN entry
                    // on the pairing screen, IP entry on the add/edit-host screens.
                    Event::KeyDown { keycode: Some(k), .. }
                        if matches!(app.screen, Screen::Pairing | Screen::AddHost | Screen::EditHost) =>
                    {
                        if let Some(digit) = crate::ui::digit_key_value(k) {
                            match app.screen {
                                Screen::Pairing => app.enter_pin_digit(digit),
                                Screen::AddHost | Screen::EditHost => app.enter_add_host_digit(digit),
                                _ => unreachable!(),
                            }
                            continue;
                        }
                    }
                    // Text committed by webOS's on-screen keyboard (see
                    // `SOFTWARE_KEYBOARD` in this module): the OSK delivers whole
                    // strings via SDL_TEXTINPUT, not synthetic key events, so it has
                    // to be consumed separately from the number-pad path above. Each
                    // character is fed through the same entry state machine, so typing
                    // "192.168.1.5" on the keyboard and tapping it out on the remote
                    // produce identical results.
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
                        continue;
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
                                tracing::info!("controller connected: {}", c.name());
                                *controller = Some(c);
                            }
                            Err(e) => tracing::warn!("controller open failed: {e}"),
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
                            if let Some(target) = app.back() {
                                break 'ui target;
                            }
                        } else if let Some(target) =
                            app.handle_home_event(menu_ev, display_mode.w as u32, display_mode.h as u32)
                        {
                            break 'ui target;
                        }
                    }
                    Screen::Pairing => app.handle_pairing_event(menu_ev),
                    Screen::Settings => app.handle_settings_event(menu_ev),
                    Screen::AddHost => app.handle_add_host_event(menu_ev),
                    Screen::Wake => app.handle_wake_event(menu_ev),
                    Screen::ForgetHost => app.handle_forget_host_event(menu_ev),
                    Screen::HostMenu => app.handle_host_menu_event(menu_ev),
                    Screen::SpeedTest => app.handle_speed_test_event(menu_ev),
                    Screen::EditHost => app.handle_edit_host_event(menu_ev),
                    Screen::About => {
                        app.handle_about_event(menu_ev, display_mode.w as u32, display_mode.h as u32, fonts)
                    }
                }
            }
            // Track whether the panel is actually up, not merely whether text input was
            // requested: webOS lets the user dismiss the keyboard while the field stays
            // focused, and the address card drops back down when that happens. A change
            // is `dirty`, since it moves the card.
            let keyboard_shown = text_input.is_screen_keyboard_shown(canvas.window());
            if keyboard_shown != app.keyboard_shown {
                app.keyboard_shown = keyboard_shown;
                dirty = true;
                tracing::info!("on-screen keyboard shown: {keyboard_shown}");
            }
            // Ask for (or dismiss) the webOS on-screen keyboard as the screen changes —
            // see `text_input_screen`. Edge-triggered: `SDL_StartTextInput` is not
            // idempotent-free on this backend (it re-shows the panel), so calling it
            // every tick would fight the user dismissing it.
            let wants_text = text_input_screen(app.screen);
            if wants_text != text_input_active {
                text_input_active = wants_text;
                if wants_text {
                    text_input.set_rect(app.address_field_rect(display_mode.w as u32, display_mode.h as u32, fonts));
                    text_input.start();
                } else {
                    text_input.stop();
                }
                // Both values, deliberately: `HasScreenKeyboardSupport` and
                // `IsScreenKeyboardShown` are separate SDL video-driver callbacks, and a
                // driver can implement the first without the second — in which case SDL
                // returns false unconditionally and the address card would never lift.
                // Logging the pair makes that distinguishable on-device in one run.
                tracing::info!(
                    "text input requested: {wants_text} (keyboard shown: {})",
                    text_input.is_screen_keyboard_shown(canvas.window())
                );
            }
            // Animations advance every 16ms tick and keep frames flowing on their
            // own; `dirty` (an event/drain changed actual content) additionally
            // forces stale tiles to re-rasterize.
            // `tiles_pending` keeps frames flowing while the card window is still being
            // filled a few tiles at a time (see `CARD_BUILD_BUDGET`) — the
            // redraw-on-change loop would otherwise go idle mid-build and leave the rest
            // of the visible cards blank until the next input.
            let animating = app.tick_animations() || app.tiles_pending;
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
            // Release textures for cards scrolled out of the keep window before uploading
            // new ones, so a long scroll frees memory in the same frame it claims more
            // rather than peaking at both.
            for tile in std::mem::take(&mut app.evicted_tiles) {
                compositor.drop_tile(tile);
            }
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
        if text_input_active {
            text_input.stop();
        }
        Ok(Some((
            target.host,
            target.port,
            Some(target.fingerprint),
            target.launch,
        )))
    }

    fn run_inner() -> Result<()> {
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
            )?
            else {
                tracing::info!("punktfunk-webos exiting cleanly");
                return Ok(());
            };

            let settings = store::load_settings();
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
            tracing::info!(
                "requesting {}x{}@{}",
                settings.width,
                settings.height,
                settings.refresh_hz
            );
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
                settings.audio_channels,
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
                settings.codec,
            ) {
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
            // Gamepad path to the disconnect dialog (see `GUIDE_HOLD`): `Some(t)` =
            // Guide pressed at `t` and still held; cleared on release or once it fires.
            let mut guide_held_since: Option<Instant> = None;
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
                            // As deliberate a stop as long-press-Back below — tear the
                            // virtual display down now instead of lingering for a reconnect.
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
                        _ if disconnect_dialog.is_some() => {
                            let focus = disconnect_dialog.expect("guarded by is_some above");
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
                                    disconnect_dialog = Some(1 - focus);
                                    disconnect_focus_dirty = true;
                                    disconnect_focus_anim = Some(Instant::now());
                                }
                                Some(MenuEvent::Confirm) if focus == 0 => {
                                    tracing::info!("back — disconnecting to menu");
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
                        Event::KeyDown {
                            keycode: Some(k),
                            scancode: None,
                            repeat: false,
                            ..
                        } if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back) => {
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
                if disconnect_dialog.is_none() {
                    if let Some(since) = guide_held_since {
                        if since.elapsed() >= GUIDE_HOLD {
                            guide_held_since = None;
                            disconnect_dialog = Some(1);
                            disconnect_shell_dirty = true;
                            disconnect_focus_dirty = true;
                            disconnect_focus_anim = Some(Instant::now());
                        }
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
                            DrawCmd::Fill {
                                rect: full,
                                color: sdl2::pixels::Color::RGBA(0, 0, 0, crate::ui::MODAL_SCRIM.a),
                            },
                            DrawCmd::Tex {
                                tile: Tile::DisconnectDialog,
                                dst: full,
                                alpha: 0xff,
                            },
                            DrawCmd::Tex {
                                tile: Tile::DisconnectFocusButton,
                                dst: crate::ui::zoom_rect(base, f, 0.02),
                                alpha: 0xff,
                            },
                        ],
                    )?;
                    canvas.present();
                }
                // Offloaded audio is drained by its own dedicated thread instead (see
                // `session::ndl_audio_pump`) — nothing to do here.
                if let Some(player) = &mut audio_player {
                    session::pump_audio_once(&connected.client, player);
                }
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
                    let feed_ms = connected.stats.feed_us.load(Ordering::Relaxed) as f32 / 1000.0;
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
                                "Dropped {} · hold {} · backlog {backlog}",
                                connected.client.frames_dropped(),
                                if holding { "yes" } else { "no" },
                            )
                        },
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
                        Err(e) => tracing::warn!("stats overlay render failed: {e:#}"),
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
