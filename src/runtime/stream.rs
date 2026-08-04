use std::sync::Arc;

use super::*;

pub(super) fn run_inner() -> Result<()> {
    // Stops webOS's launcher intercepting Back/Windows-Meta/Guide as its own Home shortcut
    // (see `keyboard.rs`'s LGui/RGui and `gamepad.rs`'s BTN_GUIDE mapping). Must be set before
    // window creation — these hints only latch at creation time.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "true");
    // Without this webOS SIGTERMs the app on a held/root-level Back before it can react.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_EXIT", "true");
    // Shares the Home-class policy with the remote's Home button (no separate policy exists);
    // capturing is the only way a keyboard's Super key reaches the app to forward as
    // VK_LWIN/VK_RWIN. Cost: remote Home no longer opens the launcher natively, so the input
    // loop detects it (bare keycode, no scancode) and relaunches via `luna::launch_home`.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_HOME", "true");
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_META", "true");
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_GUIDE", "true");
    // Suppress webOS's launcher ribbon popping over the foregrounded app.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_RIBBON", "false");
    // Linear texture filtering — the focus-pop scale shimmers on SDL's default nearest.
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

    // Pre-stream UI backend: tiny-skia rasterizes cached widget tiles, GPU composites them
    // each frame — see `compositor.rs`.
    let mut compositor = Compositor::new();

    let mut events = sdl.event_pump().map_err(|e| anyhow::anyhow!("event pump: {e}"))?;

    let identity = store::load_or_create_identity().context("load_or_create_identity")?;

    // Sized for a 10-foot TV viewing distance.
    let text_raster = crate::platform::webos::text_sdl::SdlTextRaster::new(&ttf, display_mode.h as u32)?;
    let fonts = crate::ui::Fonts {
        raster: &text_raster,
        label: crate::ui::FontId::Label,
        value: crate::ui::FontId::Value,
        title: crate::ui::FontId::Title,
        icon: crate::ui::FontId::Icon,
        caption: crate::ui::FontId::Caption,
    };

    // Owned above the loop, not re-declared per iteration: `ControllerDeviceAdded` fires only
    // once per physical (re)connection, so a pad opened earlier must carry across screens.
    let mut controller: Option<GameController> = None;
    // Why the *last* stream attempt bounced to the menu, shown on the fresh Home screen.
    let mut menu_status: Option<String> = None;
    // Same, but for a toast popup (e.g. the host closed the session) instead of the
    // bottom status line — shown on the Home screen right after re-entering the menu.
    let mut menu_toast: Option<String> = None;

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
            menu_toast.take(),
        )?
        else {
            tracing::info!("punktfunk-webos exiting cleanly");
            return Ok(());
        };
        tracing::debug!("settings: {settings:?}");

        // `hide()` unmaps the surface entirely, silently breaking the Magic Remote's pointer
        // forwarding since Wayland has nowhere left to route motion. aurora-tv never hides its
        // window either — stays mapped, cleared fully transparent so the video shows through.
        canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
        canvas.clear();
        canvas.present();
        // Release UI GPU textures before the stream takes the GPU; re-populated on menu return.
        tracing::debug!("releasing all compositor textures for stream handoff");
        compositor.clear_all();
        // Local pointer hidden unless "Cursor capture" is off — otherwise it and the host's own
        // forwarded-position cursor read as "the pointer doesn't match the mouse".
        let mut cursor = cursor::Cursor::new(sdl.mouse());
        cursor.set_captured(settings.cursor_capture);

        // Already running (started in `run_ui_flow`, overlapping the launch zoom/fade) — joining
        // just waits out whatever handshake is left.
        let connected = match connect_thread.join().expect("connect thread panicked") {
            Ok(c) => c,
            Err(e) => {
                // Return to the menu with the reason on screen instead of `?`-ing the app down.
                tracing::error!("session connect failed: {e:#}");
                cursor.set_captured(false);
                menu_status = Some(format!("Couldn't connect: {}", crate::errors::friendly(&e)));
                continue;
            }
        };
        tracing::info!("session connected, entering event loop");

        // Skipped when NDL took the Opus stream — a second unfed audio device would still
        // claim a PulseAudio sink.
        let mut audio_player = if connected.audio_offloaded {
            None
        } else {
            match crate::platform::webos::audio::AudioPlayer::new(&sdl_audio, connected.client.audio_channels) {
                Ok(p) => Some(p),
                Err(e) => {
                    // Same no-crash policy as the connect above, plus the video teardown a
                    // loaded decoder now needs.
                    tracing::error!("audio player init failed: {e:#}");
                    connected.client.disconnect_quit();
                    if connected.shutdown() {
                        crate::platform::webos::ndl::quit();
                    } else {
                        tracing::warn!("session teardown timed out — skipping NDL unload for this run");
                    }
                    cursor.set_captured(false);
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

        // Experimental: Game picture/sound mode, app-plane stand-in for HDMI ALLM. Best-effort;
        // reverted on stream exit. See `game_mode`.
        let restore_tv_modes = if settings.game_mode {
            crate::platform::webos::game_mode::enter(connected.hdr)
        } else {
            Vec::new()
        };

        // DualSense HID feedback (adaptive triggers, lightbar), started only for an actual
        // DualSense — anything else never emits these events. Not found (USB pad, no
        // `luna-send-pub`) isn't an error: logged once, feature just stays off.
        let mut ds_feedback = if settings.gamepad_type.is_dualsense() {
            match crate::platform::webos::dualsense::find_address() {
                Some(addr) => crate::platform::webos::dualsense::Feedback::new(addr),
                None => {
                    tracing::info!(
                        "no Bluetooth DualSense found in /proc/bus/input/devices — \
                             adaptive triggers off for this session"
                    );
                    None
                }
            }
        } else {
            None
        };

        let mut scroll_acc = mouse::ScrollAccumulator::default();
        // Raw evdev mouse, only under cursor capture (uncaptured, raw deltas can't express the
        // remote's absolute pointing). Sends happen on the reader thread, not queued for the
        // ~2ms main loop, to avoid re-resampling a 1000 Hz mouse. `forward_hid` gates sends
        // since `disconnect.is_open()` isn't readable from that thread.
        let forward_hid = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hid_mouse = if settings.cursor_capture {
            let client = Arc::clone(&connected.client);
            let gate = Arc::clone(&forward_hid);
            crate::platform::webos::evmouse::HidMouse::start(move |ev| {
                if gate.load(Ordering::Relaxed) {
                    let _ = session::send_input(&client, ev);
                }
            })
        } else {
            None
        };
        // Flips once a HID mouse is found — `HidMouse::start` no longer scans before returning
        // (that blocked every stream connect on the node-open cost), so presence is only known
        // once the reader thread's own scan catches up; checked each tick below.
        let mut hid_device_seen = false;
        // Stats overlay: refreshed ~2Hz onto the transparent stream window, over the
        // punch-through video plane via per-pixel alpha — window is never shown/hidden (that
        // crashed an earlier attempt, see docs/NOTES.md). Green button flips it live, session-only.
        let mut stats_enabled = settings.stats_overlay;
        // Fades in/out on the same curve as the toast below — see `ModalFade::visibility_alpha`.
        let mut stats_fade = crate::ui::ModalFade::<()>::new();
        if stats_enabled {
            stats_fade.open();
        }
        let mut log_fade = crate::ui::ModalFade::<()>::new();
        let mut green_held = false;
        let mut yellow_held = false;
        let mut home_held = false;
        // Blue button flips pacing live via `stats.pacing_enabled` (pure PTS math, safe mid-stream).
        let mut blue_held = false;
        // Transient toasts. `overlay_was_active` catches the fade-out edge so the canvas gets
        // wiped once; `stats_dst`/`log_dst` recomposite each frame at their own slower cadence.
        let mut notif = crate::ui::Notification::new();
        // Last (text, w, h) uploaded for the toast tile — see `push_notification_cmd`.
        let mut notif_tile: Option<(String, u32, u32)> = None;
        // Edge-detects `stats.holding` (freeze-until-reanchor — see `session::video_pump`) so a
        // packet-loss stall surfaces as a toast even with the stats overlay off, same signal the
        // overlay's "Beat" line already reads.
        let mut was_holding = false;
        let mut overlay_was_active = false;
        let mut stats_dst: Option<crate::ui::render::Rect> = None;
        let mut log_dst: Option<crate::ui::render::Rect> = None;
        let mut stats_built_at: Option<Instant> = None;
        let mut overlay_last: Option<Instant> = None;
        let mut overlay_prev_frames: u64 = 0;
        let mut overlay_prev_bytes: u64 = 0;
        let mut overlay_prev_cpu_ticks: Option<u64> = None;
        let mut overlay_prev_at = Instant::now();
        // 0 = "Disconnect" focused, 1 = "Cancel" (default on open — safer).
        let mut disconnect = ConfirmDialog::new(
            "Stop streaming?",
            "The stream will end and you'll return to the menu.",
            crate::ui::confirm_buttons(Some(crate::ui::ICON_CLOSE), "Stop streaming", crate::ui::ERROR_RED),
        );
        // Gamepad routes to the disconnect dialog — see `DisconnectChord`.
        let mut chord = DisconnectChord::default();
        // Short Back tap forwards Esc; a held Back becomes webOS's EXIT gesture, polled below.
        let mut exit_held = false;
        // Waits for close-fade to finish.
        let mut pending_outcome: Option<StreamOutcome> = None;
        // Set when the user confirms the disconnect dialog — distinguishes that from the
        // host ending the session or the network dropping out, so the toast below only
        // fires for the latter. (SIGTERM/window-close also call `disconnect_quit()`, but
        // those break with `StreamOutcome::Quit` and never reach the check below.)
        let mut client_initiated_disconnect = false;
        let outcome = 'running: loop {
            if QUIT_REQUESTED.load(Ordering::Relaxed) {
                tracing::warn!("SIGTERM/SIGINT received — disconnecting before exit");
                connected.client.disconnect_quit();
                break 'running StreamOutcome::Quit;
            }
            if !hid_device_seen
                && hid_mouse
                    .as_ref()
                    .is_some_and(crate::platform::webos::evmouse::HidMouse::has_device)
            {
                hid_device_seen = true;
                cursor.disable_sdl_relative();
            }
            for event in events.poll_iter() {
                use sdl2::event::Event;
                // While the HID reader is seeing mouse reports, SDL's pointer events are that
                // same input echoed by the compositor and must be dropped; once it idles, what
                // arrives is the remote. Read once per event, not per guard — the window is
                // 250ms, so per-arm freshness buys nothing.
                let (hid_motion, hid_clicks) = match hid_mouse.as_ref() {
                    Some(hid) => (hid.owns_sdl_motion(), hid.owns_sdl_clicks()),
                    None => (false, false),
                };
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
                        // An unplugged pad sends no releases, so a held chord would stay armed forever.
                        chord.clear();
                    }
                    // Dialog open: navigate it only, don't forward input to the host.
                    _ if disconnect.is_open() => {
                        match disconnect.handle_event(&event, display_mode.w as u32, display_mode.h as u32, &fonts) {
                            Some(ConfirmAction::Confirmed) => {
                                tracing::info!("disconnecting to menu");
                                client_initiated_disconnect = true;
                                connected.client.disconnect_quit();
                                disconnect.dismiss();
                                pending_outcome = Some(StreamOutcome::ReturnToMenu);
                            }
                            Some(ConfirmAction::Dismissed) => overlay_last = None,
                            Some(ConfirmAction::Navigated) | None => {}
                        }
                    }
                    // Scancode keys are real game input — forward only, never open the dialog.
                    Event::KeyDown { scancode: Some(sc), .. } => {
                        if let Some(ev) = keyboard::key_event(sc, true) {
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    // Magic Remote Back has no scancode — forwarded as Esc. A held Back never
                    // arrives here; webOS delivers it as the EXIT gesture polled below instead.
                    Event::KeyDown {
                        keycode: Some(k),
                        scancode: None,
                        repeat: false,
                        ..
                    } if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) => {
                        if let Some(ev) = keyboard::key_event(sdl2::keyboard::Scancode::Escape, true) {
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    Event::KeyUp {
                        keycode: Some(k),
                        scancode: None,
                        ..
                    } if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) => {
                        if let Some(ev) = keyboard::key_event(sdl2::keyboard::Scancode::Escape, false) {
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    Event::KeyUp { scancode: Some(sc), .. } => {
                        if let Some(ev) = keyboard::key_event(sc, false) {
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    Event::ControllerButtonDown { button, .. } => {
                        chord.set(button, true);
                        // Still forwarded: the hold requirement is what keeps game input and
                        // the shortcut apart.
                        let ev = gamepad::button_event(button, true, 0);
                        let _ = session::send_input(&connected.client, &ev);
                    }
                    Event::ControllerButtonUp { button, .. } => {
                        chord.set(button, false);
                        let ev = gamepad::button_event(button, false, 0);
                        let _ = session::send_input(&connected.client, &ev);
                    }
                    Event::ControllerAxisMotion { axis, value, .. } => {
                        let ev = gamepad::axis_event(axis, value, 0);
                        let _ = session::send_input(&connected.client, &ev);
                    }
                    // Magic Remote pointer mode surfaces as plain SDL2 mouse events, forwarded
                    // to the host instead of driving local UI focus (see `mouse.rs`).
                    Event::MouseMotion { x, y, xrel, yrel, .. } => {
                        cursor.on_pointer_activity();
                        if !hid_motion {
                            // Relative only for the remote alone: SDL's warp emulation is off
                            // whenever the evdev reader owns motion, so the remote sends
                            // absolute — also the better fit for a device the user aims.
                            let ev = if settings.cursor_capture && !hid_device_seen {
                                mouse::move_relative_event(xrel, yrel)
                            } else {
                                mouse::move_event(x, y, display_mode.w as u32, display_mode.h as u32)
                            };
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    Event::MouseButtonDown { mouse_btn, .. } if !hid_clicks => {
                        if let Some(ev) = mouse::button_event(mouse_btn, true) {
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    Event::MouseButtonUp { mouse_btn, .. } if !hid_clicks => {
                        if let Some(ev) = mouse::button_event(mouse_btn, false) {
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    Event::MouseWheel { x, y, .. } if !hid_clicks => {
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
            // Chord held long enough — open the dialog, then forget it so it fires once per hold.
            if !disconnect.is_open() && chord.held_for(EXIT_HOLD) {
                tracing::info!("disconnect shortcut held — opening dialog");
                chord.clear();
                disconnect.open(1);
            }
            // EXIT gesture (held Back) opens the dialog; a short tap is Esc, above.
            if exit_gesture_fired(&mut exit_held) && !disconnect.is_open() {
                tracing::info!("EXIT gesture — opening disconnect dialog");
                disconnect.open(1);
            }
            // Re-opens the webOS launcher; a long Back fires EXIT above, never this.
            if home_key_fired(&mut home_held) {
                crate::platform::webos::luna::launch_home();
            }
            // Green button: stats-overlay toggle, edge-detected via raw scancode poll (the
            // safe SDL2 event API can't see this key). Skipped while the dialog owns input.
            let green_down = !disconnect.is_open()
                && crate::platform::webos::input::webos_scancode_down(
                    crate::platform::webos::input::WEBOS_GREEN_SCANCODE,
                );
            if green_down && !green_held {
                stats_enabled = !stats_enabled;
                overlay_last = None; // force an immediate redraw
                if stats_enabled {
                    stats_fade.reopen();
                } else {
                    stats_fade.close(());
                }
            }
            green_held = green_down;
            // Yellow button: log-tail overlay Off -> Live -> Frozen -> Off, same edge-detect
            // as Green above; also handled in `run_ui_flow` for non-streaming screens.
            let yellow_down = !disconnect.is_open()
                && crate::platform::webos::input::webos_scancode_down(
                    crate::platform::webos::input::WEBOS_YELLOW_SCANCODE,
                );
            if yellow_down && !yellow_held {
                let was_on = log_overlay_state() != LogOverlayState::Off;
                cycle_log_overlay();
                let now_on = log_overlay_state() != LogOverlayState::Off;
                overlay_last = None; // force an immediate redraw with the new state
                if now_on && !was_on {
                    log_fade.reopen();
                } else if was_on && !now_on {
                    log_fade.close(());
                }
            }
            yellow_held = yellow_down;
            // Blue button: live frame-pacing toggle, same edge-detect (Red is OS-intercepted).
            let blue_down = !disconnect.is_open()
                && crate::platform::webos::input::webos_scancode_down(
                    crate::platform::webos::input::WEBOS_BLUE_SCANCODE,
                );
            if blue_down && !blue_held {
                let now_on = !connected.stats.pacing_enabled.load(Ordering::Relaxed);
                connected.stats.pacing_enabled.store(now_on, Ordering::Relaxed);
                tracing::info!("frame pacing {} (Blue button)", if now_on { "on" } else { "off" });
                notif.show(if now_on {
                    "Frame pacing enabled"
                } else {
                    "Frame pacing disabled"
                });
                overlay_last = None;
            }
            blue_held = blue_down;
            // Connection-issue toast: fires on the rising edge of a freeze-until-reanchor hold
            // (dropped/gapped frames — see `session::video_pump`), which is the same "network
            // trouble" signal the stats overlay's "Beat" line reads, just edge-triggered here so
            // it's visible without the overlay open. No matching "recovered" toast — the video
            // itself resuming is the recovery signal.
            let holding_now = connected.stats.holding.load(Ordering::Relaxed);
            if holding_now && !was_holding {
                tracing::warn!("connection issues detected (freeze-until-reanchor)");
                notif.show("Connection issues — recovering...");
                overlay_last = None;
            }
            was_holding = holding_now;
            // The dialog is navigated with the Magic Remote's pointer, so a captured stream
            // must hand the pointer back while it's up — hidden/relative there'd be nothing
            // to aim with. Recaptured on dismiss.
            let want_captured = settings.cursor_capture && !disconnect.is_open();
            if want_captured != cursor.is_captured() {
                cursor.set_captured(want_captured);
                forward_hid.store(want_captured, Ordering::Relaxed);
            }
            // Wider than `is_open()`: a dismissed dialog still draws (fading out) a few more
            // ticks, used below to skip the stats overlay for exactly those ticks.
            let dialog_frame = disconnect.frame(MODAL_FADE);
            if dialog_frame.is_some() {
                // Own clear/present pass over the punch-through video, unlike the menu's
                // shared command list.
                let mut cmds = Vec::new();
                disconnect.draw(
                    &mut compositor,
                    &texture_creator,
                    &fonts,
                    display_mode.w as u32,
                    display_mode.h as u32,
                    &mut cmds,
                )?;
                canvas.set_blend_mode(sdl2::render::BlendMode::None);
                canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                canvas.clear();
                compositor.present(&mut canvas, &cmds)?;
                canvas.present();
            } else if disconnect.fade.tick(MODAL_FADE) {
                // Close-fade just finished. Confirmed Disconnect: break now, nothing to wipe
                // since the pre-stream UI takes the canvas next.
                if let Some(outcome) = pending_outcome.take() {
                    break 'running outcome;
                }
                // Cancel/Back: wipe the last frame so it doesn't stick over the video.
                canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                canvas.clear();
                canvas.present();
                canvas.clear();
                canvas.present();
            }
            // Offloaded audio drains on its own dedicated thread instead (`session::ndl_audio_pump`).
            if let Some(player) = &mut audio_player {
                session::pump_audio_once(&connected.client, player);
            }
            // Unconditional so both feedback planes keep draining with no pad attached.
            session::pump_feedback_once(&connected.client, controller.as_mut(), ds_feedback.as_mut());
            // Skipped while the dialog owns the canvas. Stats/log share one clear/execute/present
            // so neither erases the other's tile.
            //
            // `log_overlay_lines()` deferred to the throttled block below, not called every
            // ~2ms tick — it locks the same mutex log writes contend on ~500x/s.
            let notif_frame = if dialog_frame.is_none() { notif.frame() } else { None };
            let notif_active = notif_frame.is_some();
            // Fade in/out on the toast's curve instead of cutting instantly; `visibility_alpha`
            // keeps returning `Some` through the close fade after the toggle itself flips off.
            let stats_alpha = stats_fade.visibility_alpha(crate::ui::OVERLAY_FADE, stats_enabled);
            let log_overlay_on = log_overlay_state() != LogOverlayState::Off;
            let log_alpha = log_fade.visibility_alpha(crate::ui::OVERLAY_FADE, log_overlay_on);
            let overlay_active = stats_alpha.is_some() || log_alpha.is_some() || notif_active;
            if overlay_was_active && !overlay_active {
                // Nothing else clears this canvas — the faded-out tile would stick otherwise.
                canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                canvas.clear();
                canvas.present();
                canvas.clear();
                canvas.present();
            }
            overlay_was_active = overlay_active;
            // A fade in flight needs frequent frames; steady-state stats/log are fine at ~2Hz.
            let fading = notif_active
                || stats_fade.is_animating(crate::ui::OVERLAY_FADE)
                || log_fade.is_animating(crate::ui::OVERLAY_FADE);
            let redraw_interval = if fading {
                Duration::from_millis(33)
            } else {
                Duration::from_millis(500)
            };
            if overlay_active && dialog_frame.is_none() && overlay_last.is_none_or(|t| t.elapsed() >= redraw_interval) {
                overlay_last = Some(Instant::now());
                let mut cmds: Vec<DrawCmd> = Vec::new();
                // Content stays on a 500ms cadence even when a toast fade runs the loop faster.
                if stats_enabled && stats_built_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                    stats_built_at = Some(Instant::now());
                    let frames = connected.stats.frames.load(Ordering::Relaxed);
                    let bytes = connected.stats.bytes.load(Ordering::Relaxed);
                    let dt = overlay_prev_at.elapsed().as_secs_f32().max(0.001);
                    let fps = (frames.saturating_sub(overlay_prev_frames)) as f32 / dt;
                    // Measured, vs. negotiated `resolved_bitrate_kbps`.
                    let actual_kbps = (bytes.saturating_sub(overlay_prev_bytes)) as f32 * 8.0 / 1000.0 / dt;
                    overlay_prev_frames = frames;
                    overlay_prev_bytes = bytes;
                    overlay_prev_at = Instant::now();
                    let mode = connected.client.mode();
                    let feed_ms = connected.stats.feed_us.load(Ordering::Relaxed) as f32 / 1000.0;
                    let holding = connected.stats.holding.load(Ordering::Relaxed);
                    // CPU% (one core) + RSS, only read while the overlay is up.
                    let cpu_mem_line = session::process_cpu_mem().map(|(cpu_ticks, mem_bytes)| {
                        // No baseline on the first sample, so CPU shows from the 2nd on.
                        let cpu = overlay_prev_cpu_ticks.map(|prev| {
                            let pct =
                                (cpu_ticks.saturating_sub(prev)) as f32 / session::clock_ticks_per_sec() as f32 / dt
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
                            "{}x{}@{} {}{}",
                            mode.width,
                            mode.height,
                            mode.refresh_hz,
                            session::codec_name(connected.client.codec),
                            if connected.client.color.is_hdr() { " HDR" } else { "" },
                        ),
                        format!("Video {fps:.1} fps · {frames} frames"),
                        {
                            // NDL's undecoded/unpresented depth: rising means decode is behind,
                            // flat-near-zero while stuttering means the problem is upstream.
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
                    if connected.stats.pacing_enabled.load(Ordering::Relaxed) {
                        let delta_ms = connected.stats.pacing_delta_ns.load(Ordering::Relaxed) as f32 / 1_000_000.0;
                        lines.push(format!("Pace {delta_ms:+.1} ms"));
                    }
                    match crate::ui::render_stats_overlay_tile(
                        fonts.raster,
                        fonts.value,
                        fonts.caption,
                        &lines,
                        "Press green button to hide this overlay",
                    ) {
                        Ok(tile) => {
                            let (tw, th) = (tile.width(), tile.height());
                            compositor.upload(&texture_creator, Tile::StatsOverlay, &tile)?;
                            stats_dst = Some(crate::ui::render::Rect::new(
                                display_mode.w - tw as i32 - 24,
                                24,
                                tw,
                                th,
                            ));
                        }
                        Err(e) => tracing::warn!("stats overlay render failed: {e:#}"),
                    }
                }
                if let Some(alpha) = stats_alpha {
                    if let Some(dst) = stats_dst {
                        cmds.push(DrawCmd::Tex {
                            tile: Tile::StatsOverlay,
                            dst,
                            alpha: (alpha * 255.0) as u8,
                        });
                    }
                }
                // `None` during fade-out once the toggle flips Off — the fade keeps
                // recompositing the last uploaded tile via `log_dst`.
                if let Some(lines) = log_overlay_lines() {
                    match crate::ui::render_log_overlay_tile(fonts.raster, fonts.caption, display_mode.w as u32, &lines)
                    {
                        Ok(tile) => {
                            let (tw, th) = (tile.width(), tile.height());
                            compositor.upload(&texture_creator, Tile::LogOverlay, &tile)?;
                            log_dst = Some(crate::ui::render::Rect::new(0, display_mode.h - th as i32, tw, th));
                        }
                        Err(e) => tracing::warn!("log overlay render failed: {e:#}"),
                    }
                }
                if let Some(alpha) = log_alpha {
                    if let Some(dst) = log_dst {
                        cmds.push(DrawCmd::Tex {
                            tile: Tile::LogOverlay,
                            dst,
                            alpha: (alpha * 255.0) as u8,
                        });
                    }
                }
                push_notification_cmd(&mut compositor, &texture_creator, &fonts, &notif_frame, display_mode.w, &mut notif_tile, &mut cmds)?;
                if !cmds.is_empty() {
                    canvas.set_blend_mode(sdl2::render::BlendMode::None);
                    canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 0));
                    canvas.clear();
                    compositor.present(&mut canvas, &cmds)?;
                    canvas.present();
                }
            }
            if connected.client.is_session_ended() {
                tracing::info!("host ended the session");
                // `is_session_ended` covers both a graceful host close and a network
                // drop/idle-timeout (see `NativeClient::is_session_ended`'s doc) — no
                // signal distinguishes them, so one message covers both. But it also
                // flips true right after *our own* `disconnect_quit()` calls above
                // (Back/dialog, SIGTERM) — skip the toast for those, it's not news to
                // the user who just asked to disconnect.
                if !client_initiated_disconnect {
                    menu_toast = Some("The host closed the connection".to_string());
                }
                break 'running StreamOutcome::ReturnToMenu;
            }

            // Bounds staleness of forwarded input/audio (video has its own thread). 2ms keeps
            // added latency near zero; the wakeup rate is noise even on this SoC.
            std::thread::sleep(Duration::from_millis(2));
        };

        // Trigger resistance is firmware state that outlives the session — hand the pad back
        // first or a game that ended with R2 stiff leaves it stiff on the TV home screen.
        if let Some(mut fb) = ds_feedback.take() {
            fb.release();
        }
        // Rumble is likewise pad state, not stream state.
        if let Some(pad) = controller.as_mut() {
            let _ = pad.set_rumble(0, 0, 0);
        }
        // `shutdown()` joins the video thread and drops `client` so the QUIC close frame
        // actually sends. `false` means a teardown thread is wedged in FFI — skip the NDL
        // unload rather than race it, and accept the leak for this run.
        if connected.shutdown() {
            crate::platform::webos::ndl::quit();
        } else {
            tracing::warn!("session teardown timed out — skipping NDL unload for this run");
        }
        // Put the TV's picture/sound modes back (no-op unless game mode switched them).
        crate::platform::webos::game_mode::restore(restore_tv_modes);
        cursor.set_captured(false);
        match outcome {
            StreamOutcome::Quit => {
                tracing::info!("punktfunk-webos exiting cleanly");
                return Ok(());
            }
            StreamOutcome::ReturnToMenu => continue,
        }
    }
}
