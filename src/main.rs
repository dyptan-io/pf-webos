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
mod ndl;
#[cfg(target_os = "linux")]
mod gamepad;
#[cfg(target_os = "linux")]
mod library;
#[cfg(target_os = "linux")]
mod mouse;
#[cfg(target_os = "linux")]
mod session;
#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
mod ui;

#[cfg(target_os = "linux")]
mod real {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use punktfunk_core::config::Mode;

    use crate::app::{App, Screen};
    use crate::gamepad;
    use crate::mouse;
    use crate::session;
    use crate::store;
    use crate::ui::MenuEvent;

    /// What `run_ui_flow` resolved: host, port, the pinned fingerprint (`None` for a
    /// fresh TOFU connect), and an optional library entry id to launch into.
    type ConnectOutcome = (String, u16, Option<[u8; 32]>, Option<String>);

    /// webOS native apps run with no attached terminal; `dev-manager-desktop`'s log
    /// viewer reads this file. `/media/developer/apps/usr/palm/applications/<appid>/`
    /// is the app's own writable directory (falls back to `/tmp` off-device, e.g. when
    /// smoke-testing this binary on a Linux dev box before packaging).
    fn log_path() -> PathBuf {
        std::env::var_os("PUNKTFUNK_WEBOS_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("punktfunk-webos.log")
    }

    pub fn run() -> Result<()> {
        let log_path = log_path();
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open log file {}", log_path.display()))?;
        writeln!(log, "punktfunk-webos starting")?;

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

    /// aurora-tv's confirmed webOS NTSC correction (`app_settings.c`,
    /// `settings_ntsc_refresh_rate_x100_for_fps`): real LG panels run 1000/1001 ×
    /// the nominal rate (30/60/120/240 only), floored to a whole Hz for the wire
    /// value since punktfunk's `Mode.refresh_hz` has no centihertz field like
    /// Limelight's `clientRefreshRateX100`. 60 → 59, 120 → 119.
    fn ntsc_correct(nominal_hz: u32) -> u32 {
        match nominal_hz {
            30 | 60 | 120 | 240 => ((nominal_hz as u64 * 1000 / 1001) as u32).max(1),
            other => other,
        }
    }

    /// How long the *keyboard*/*gamepad* Back-equivalent must be held during a
    /// stream before it disconnects and returns to the menu — long enough that a
    /// normal game-input tap of the same physical button (many games use
    /// B/Back-ish buttons) never triggers it. The Magic Remote's own physical Back
    /// (`ui::webos_back_button_down`)/Red (`webos_red_button_down`) keys need no
    /// such hold: they're never forwarded to the host as game input, so an
    /// immediate press is unambiguous.
    const LONG_PRESS_BACK: Duration = Duration::from_millis(1500);

    /// How long the Magic Remote's real Back/Red must be held (continuously, checked
    /// live during the hold rather than waiting for release) before quitting the app
    /// outright, rather than just disconnecting to the menu — this app's own timer,
    /// not the system's (see `run_inner`'s docs on why relying on webOS's own
    /// long-press/Exit gesture turned out unreliable). Distinctly longer than
    /// `LONG_PRESS_BACK` so it reads as a deliberate "hold much longer to quit", not
    /// an accidental extension of a plain disconnect.
    const HW_BACK_HOLD_QUITS: Duration = Duration::from_millis(2500);

    enum StreamOutcome {
        /// The system asked the app to close (not just this stream) — exit fully.
        Quit,
        /// The host ended the session, or the user held Back — go back to the
        /// host-list/settings UI instead of exiting the app.
        ReturnToMenu,
    }

    /// Applies a `Back` to whichever screen is current — shared by the normal
    /// keyboard/gamepad dispatch and the raw scancode poll below (the real Back
    /// button, `ui::webos_back_button_down`, plus Red as a secondary trigger,
    /// `ui::webos_red_button_down` — kept since the access-policy hint that lets
    /// Back reach the app isn't honored consistently across every firmware/model,
    /// see `docs/NOTES.md`).
    fn apply_back(app: &mut App, log: &mut std::fs::File) -> Option<crate::app::ConnectTarget> {
        match app.screen {
            // Home has nothing to "back out" of (it's the root screen) — Back is a
            // shortcut straight to Settings instead, since it's otherwise reachable
            // only via the sidebar's trailing row.
            Screen::Home => {
                app.screen = Screen::Settings;
                app.dropdown = None;
                app.settings_focused = 0;
                None
            }
            Screen::Pairing => {
                app.handle_pairing_event(MenuEvent::Back, log);
                None
            }
            Screen::Settings => {
                app.handle_settings_event(MenuEvent::Back);
                None
            }
            Screen::AddHost => {
                app.handle_add_host_event(MenuEvent::Back);
                None
            }
        }
    }

    /// Runs the UI (host list -> pairing -> settings) until the user confirms a
    /// connect target or the system asks the app to close (`None`). A plain
    /// function, not a closure — a closure capturing `canvas`/`events` by
    /// reference would hold that borrow for as long as the closure value exists,
    /// which conflicts with using them again in the streaming loop right after.
    #[allow(clippy::too_many_arguments)]
    fn run_ui_flow(
        canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
        texture_creator: &sdl2::render::TextureCreator<sdl2::video::WindowContext>,
        events: &mut sdl2::EventPump,
        game_controller: &sdl2::GameControllerSubsystem,
        identity: &(String, String),
        display_mode: sdl2::video::DisplayMode,
        font_label: &sdl2::ttf::Font,
        font_value: &sdl2::ttf::Font,
        font_title: &sdl2::ttf::Font,
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
        // Cover-art textures aren't `Send` (they borrow `texture_creator`, tied to
        // this thread) — `app`'s art loader only ever hands over raw RGBA (see
        // `art.rs`); turning those into textures happens here, each tick.
        let mut art_textures: std::collections::HashMap<String, sdl2::render::Texture> = std::collections::HashMap::new();
        let mut back_prev = false;
        let target = 'ui: loop {
            app.drain_discovery();
            app.drain_art();
            for (id, (w, h, rgba)) in &app.art_pixels {
                if art_textures.contains_key(id) {
                    continue;
                }
                let mut texture = match texture_creator.create_texture_static(sdl2::pixels::PixelFormatEnum::RGBA32, *w, *h) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if texture.update(None, rgba, (*w * 4) as usize).is_err() {
                    continue;
                }
                texture.set_blend_mode(sdl2::render::BlendMode::Blend);
                art_textures.insert(id.clone(), texture);
            }
            for event in events.poll_iter() {
                use sdl2::event::Event;
                if let Event::Quit { .. } = event {
                    writeln!(log, "quit during UI")?;
                    return Ok(None);
                }
                // The Magic Remote's pointer mode surfaces as plain SDL2 mouse
                // events — hover updates focus, a click confirms whatever's
                // focused (matches gamepad/remote Confirm behavior).
                match event {
                    Event::MouseMotion { x, y, .. } => {
                        app.handle_mouse_motion(x, y, display_mode.w as u32, display_mode.h as u32);
                        continue;
                    }
                    Event::MouseButtonDown {
                        mouse_btn: sdl2::mouse::MouseButton::Left,
                        ..
                    } => {
                        if let Some(target) = app.handle_mouse_click(log) {
                            break 'ui target;
                        }
                        continue;
                    }
                    // Direct digit entry via the remote's number buttons — PIN entry
                    // on the pairing screen, IP:port entry on the add-host screen.
                    Event::KeyDown {
                        keycode: Some(k), ..
                    } if matches!(app.screen, Screen::Pairing | Screen::AddHost) => {
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
                    Event::KeyDown {
                        keycode: Some(k), ..
                    } => crate::ui::menu_event_for_key(k),
                    Event::ControllerButtonDown { button, .. } => {
                        crate::ui::menu_event_for_button(button)
                    }
                    Event::ControllerDeviceAdded { which, .. } => {
                        let _ = game_controller.open(which);
                        None
                    }
                    _ => None,
                };
                let Some(menu_ev) = menu_ev else { continue };
                match app.screen {
                    // A keyboard/gamepad Back is a bonus shortcut to Settings; the
                    // sidebar's own Settings row (reachable via Up/Down + Confirm,
                    // or the Red-button poll below) is the reliable primary path.
                    Screen::Home => {
                        if menu_ev == MenuEvent::Back {
                            app.screen = Screen::Settings;
                            app.dropdown = None;
                            app.settings_focused = 0;
                        } else if let Some(target) = app.handle_home_event(menu_ev, display_mode.w as u32, log) {
                            break 'ui target;
                        }
                    }
                    Screen::Pairing => app.handle_pairing_event(menu_ev, log),
                    Screen::Settings => app.handle_settings_event(menu_ev),
                    Screen::AddHost => app.handle_add_host_event(menu_ev),
                }
            }
            // Raw scancode poll (see `ui::webos_back_button_down`/`webos_red_button_down`
            // docs) — done *after* draining `poll_iter()` above so
            // `SDL_GetKeyboardState`'s snapshot reflects this tick's freshest pump, not
            // the previous tick's. No hold-to-quit tier in the menu (see the streaming
            // loop for that) — a plain Back here just navigates.
            let back_now = crate::ui::webos_back_button_down() || crate::ui::webos_red_button_down();
            if back_now && !back_prev {
                if let Some(target) = apply_back(&mut app, log) {
                    break 'ui target;
                }
            }
            back_prev = back_now;
            app.render(
                canvas,
                texture_creator,
                font_label,
                font_value,
                font_title,
                &art_textures,
                display_mode.w as u32,
                display_mode.h as u32,
            )?;
            std::thread::sleep(Duration::from_millis(16));
        };
        Ok(Some((target.host, target.port, Some(target.fingerprint), target.launch)))
    }

    fn run_inner(log: &mut std::fs::File) -> Result<()> {
        // Without this, webOS's system launcher intercepts the Magic Remote's Back
        // button before this app's event queue ever sees it — the app just gets
        // backgrounded to the launcher's app grid instead of getting a key event to
        // handle itself. `webosbrew/SDL-webOS` (confirmed via its source,
        // `src/video/wayland/SDL_waylandwebos.c`) exposes exactly this as a hint,
        // consumed via `wl_webos_shell_surface_set_property` when the window's shell
        // surface is set up — must be set before `video.window(...).build()` below.
        // Harmless no-op on a non-webOS SDL2 (an unrecognized hint name is just
        // ignored), so this doesn't need a target cfg.
        //
        // Deliberately NOT also setting `SDL_WEBOS_ACCESS_POLICY_KEYS_EXIT`: live
        // testing showed that once it's on, a plain short Back press stops arriving
        // as its own event at all (the system seems to buffer/withhold it while
        // deciding whether it's the start of a long-press, and apparently only ever
        // delivers *one* outcome) — so this app times the hold itself instead, same
        // proven mechanism already used for the keyboard/gamepad Back-equivalent (see
        // `LONG_PRESS_BACK` below), just fed by the real Back scancode too now.
        sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "1");

        let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL_Init: {e}"))?;
        let ttf = sdl2::ttf::init().map_err(|e| anyhow::anyhow!("SDL_ttf init: {e}"))?;
        let video = sdl
            .video()
            .map_err(|e| anyhow::anyhow!("SDL video subsystem: {e}"))?;
        let game_controller = sdl
            .game_controller()
            .map_err(|e| anyhow::anyhow!("SDL game controller subsystem: {e}"))?;
        let sdl_audio = sdl
            .audio()
            .map_err(|e| anyhow::anyhow!("SDL audio subsystem: {e}"))?;
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

        let mut events = sdl
            .event_pump()
            .map_err(|e| anyhow::anyhow!("event pump: {e}"))?;

        let identity = store::load_or_create_identity().context("load_or_create_identity")?;

        // Sized for a 10-foot TV viewing distance — see ui.rs's ROW_H/ROW_MAX_W docs.
        let font_label = crate::ui::load_font(&ttf, display_mode.h as u32, 22)?;
        let font_value = crate::ui::load_font(&ttf, display_mode.h as u32, 20)?;
        let font_title = crate::ui::load_font(&ttf, display_mode.h as u32, 40)?;

        loop {
            let Some((host, port, fp, launch)) = run_ui_flow(
                &mut canvas,
                &texture_creator,
                &mut events,
                &game_controller,
                &identity,
                display_mode,
                &font_label,
                &font_value,
                &font_title,
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
            // 0; see store::Settings::default) is what actually drives the wire value,
            // through the aurora-tv NTSC correction (see `ntsc_correct` docs).
            let wire_refresh_hz = ntsc_correct(settings.refresh_hz);
            writeln!(
                log,
                "requesting {}x{}@{} (wire refresh {wire_refresh_hz}, NTSC-corrected)",
                settings.width, settings.height, settings.refresh_hz
            )?;
            let mode = Mode {
                width: settings.width,
                height: settings.height,
                refresh_hz: wire_refresh_hz,
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

            let mut controller = None;
            let mut back_held_since: Option<Instant> = None;
            let mut hw_back_held_since: Option<Instant> = None;
            let mut hw_back_prev = false;
            let outcome = 'running: loop {
                for event in events.poll_iter() {
                    use sdl2::event::Event;
                    match event {
                        Event::Quit { .. } => break 'running StreamOutcome::Quit,
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
                        Event::KeyDown {
                            keycode: Some(k), ..
                        } if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back) => {
                            back_held_since.get_or_insert_with(Instant::now);
                        }
                        Event::KeyUp {
                            keycode: Some(k), ..
                        } if crate::ui::menu_event_for_key(k) == Some(MenuEvent::Back) => {
                            back_held_since = None;
                        }
                        Event::ControllerButtonDown { button, .. } => {
                            if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Back) {
                                back_held_since.get_or_insert_with(Instant::now);
                            }
                            let ev = gamepad::button_event(button, true, 0);
                            let _ = session::send_input(&connected.client, &ev);
                        }
                        Event::ControllerButtonUp { button, .. } => {
                            if crate::ui::menu_event_for_button(button) == Some(MenuEvent::Back) {
                                back_held_since = None;
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
                                let ev = mouse::scroll_event(y, false);
                                let _ = session::send_input(&connected.client, &ev);
                            }
                            if x != 0 {
                                let ev = mouse::scroll_event(x, true);
                                let _ = session::send_input(&connected.client, &ev);
                            }
                        }
                        _ => {}
                    }
                }
                // Raw scancode poll (see `ui::webos_back_button_down`/`webos_red_button_down`
                // docs for why neither arrives as an ordinary `Event::KeyDown`) — timed
                // entirely by this app now (see `run_inner`'s docs on why the system's
                // own long-press/Exit gesture turned out unreliable). Back/Red are never
                // forwarded to the host as game input, so unlike the keyboard/gamepad
                // path below there's no "was that a real game button" ambiguity to guard
                // against — any press disconnects on release; only a *much* longer hold
                // (`HW_BACK_HOLD_QUITS`) quits outright, checked live during the hold so
                // it doesn't need to wait for release.
                let hw_back_now = crate::ui::webos_back_button_down() || crate::ui::webos_red_button_down();
                if hw_back_now && !hw_back_prev {
                    hw_back_held_since = Some(Instant::now());
                }
                if hw_back_now && hw_back_held_since.is_some_and(|t| t.elapsed() >= HW_BACK_HOLD_QUITS) {
                    writeln!(log, "remote Back held — quitting")?;
                    connected.client.disconnect_quit();
                    break 'running StreamOutcome::Quit;
                }
                let hw_back_released = !hw_back_now && hw_back_prev && hw_back_held_since.is_some();
                hw_back_prev = hw_back_now;
                if !hw_back_now {
                    hw_back_held_since = None;
                }

                if hw_back_released || back_held_since.is_some_and(|t| t.elapsed() >= LONG_PRESS_BACK) {
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
                session::pump_audio_once(&connected.client, &mut audio_player, log);
                if connected.client.is_session_ended() {
                    writeln!(log, "host ended the session")?;
                    break 'running StreamOutcome::ReturnToMenu;
                }

                std::thread::sleep(Duration::from_millis(8));
            };

            connected.stop.store(true, std::sync::atomic::Ordering::Relaxed);
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
