use super::*;

pub(super) fn run_inner() -> Result<()> {
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
    // Distinct from KEYS_BACK: without it webOS closes (SIGTERMs) the app on its own
    // Back/exit gesture — a held or root-level Back — before the app can act on the key,
    // which is what killed the app mid-hold. aurora-tv, moonlight-tv, ihsplay and
    // RetroArch all pair EXIT with BACK for exactly this.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_EXIT", "true");
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
    let text_raster = crate::platform::webos::text_sdl::SdlTextRaster::new(&ttf, display_mode.h as u32)?;
    let fonts = crate::ui::Fonts {
        raster: &text_raster,
        label: crate::ui::FontId::Label,
        value: crate::ui::FontId::Value,
        title: crate::ui::FontId::Title,
        icon: crate::ui::FontId::Icon,
        caption: crate::ui::FontId::Caption,
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
        // as "the pointer doesn't match the remote" — hidden here unless "Cursor
        // capture" is off (see `store::Settings::cursor_capture`). Restored when back
        // in the menu (`sdl.mouse()` is the same standard SDL2 API on any platform,
        // not webOS-specific).
        sdl.mouse().show_cursor(!settings.cursor_capture);

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
            match crate::platform::webos::audio::AudioPlayer::new(&sdl_audio, connected.client.audio_channels) {
                Ok(p) => Some(p),
                Err(e) => {
                    // Same no-crash policy as the connect above — including the
                    // video-side teardown the normal stream exit does, since the
                    // connect succeeded and loaded a decoder.
                    tracing::error!("audio player init failed: {e:#}");
                    connected.client.disconnect_quit();
                    if connected.shutdown() {
                        crate::platform::webos::ndl::quit();
                    } else {
                        tracing::warn!("session teardown timed out — skipping NDL unload for this run");
                    }
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

        // Experimental: drive the TV into Game picture + sound mode (app-plane stand-in for
        // HDMI ALLM), plus max Peak Brightness on HDR, matched to the negotiated SDR/HDR path.
        // Best-effort; the returned changes are reverted on stream exit. See `game_mode`.
        let restore_tv_modes = if settings.game_mode {
            crate::platform::webos::game_mode::enter(connected.hdr)
        } else {
            Vec::new()
        };

        // DualSense HID feedback (adaptive triggers, lightbar), only when the host is
        // actually presenting a DualSense — anything else never emits these events, so
        // starting the sender thread would be pure overhead. Absent for any other reason
        // (pad on USB rather than Bluetooth, no `luna-send-pub`) is not an error: the
        // stream is unaffected, so it's logged once and the feature is simply off.
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
        // In-stream stats overlay: refreshed at ~2Hz onto the otherwise-transparent
        // stream window, composited OVER the punch-through video plane via the
        // surface's per-pixel alpha. The window is never shown/hidden here (that's
        // what crashed the old overlay attempt — see docs/NOTES.md). Starts from the
        // Settings-screen default; the Green button below flips it live for the rest
        // of this stream only, without writing back to `settings`.
        let mut stats_enabled = settings.stats_overlay;
        // Fades stats/log in on their toggle and out the same way, on the same curve
        // (`OVERLAY_FADE`) as the toast below — see `ModalFade::visibility_alpha`.
        let mut stats_fade = crate::ui::ModalFade::<()>::new();
        if stats_enabled {
            stats_fade.open();
        }
        let mut log_fade = crate::ui::ModalFade::<()>::new();
        let mut green_held = false;
        let mut yellow_held = false;
        // Blue button flips pacing live via `stats.pacing_enabled` (`video_pump` reads it
        // per frame). Pure PTS math, no decoder state — safe to toggle mid-stream.
        let mut blue_held = false;
        // Transient toasts (frame-pacing on/off, etc). `overlay_was_active` catches the
        // fade-out edge (toast, stats, or log) so the canvas gets wiped once (nothing else
        // clears it). `stats_dst`/`log_dst` let the tile recomposite every frame while its
        // content stays on a slower cadence (`stats_built_at`, and the log tail's own poll).
        let mut notif = crate::ui::Notification::new();
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
        // A short Back tap forwards Esc to the host; a held Back becomes webOS's EXIT
        // gesture (`WEBOS_EXIT_SCANCODE`), polled/edge-detected below to open the dialog.
        let mut exit_held = false;
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
                        // An unplugged pad sends no releases, so a chord held at the
                        // moment it vanished would otherwise stay armed forever.
                        chord.clear();
                    }
                    // Dialog open: navigate it only, don't forward input to the host.
                    _ if disconnect.is_open() => {
                        match disconnect.handle_event(&event, display_mode.w as u32, display_mode.h as u32, &fonts) {
                            Some(ConfirmAction::Confirmed) => {
                                tracing::info!("disconnecting to menu");
                                connected.client.disconnect_quit();
                                disconnect.dismiss();
                                pending_outcome = Some(StreamOutcome::ReturnToMenu);
                            }
                            Some(ConfirmAction::Dismissed) => overlay_last = None,
                            Some(ConfirmAction::Navigated) | None => {}
                        }
                    }
                    // Scancode keys are real game input (Backspace/Escape/etc.
                    // included) — forward only, never open the dialog.
                    Event::KeyDown { scancode: Some(sc), .. } => {
                        if let Some(ev) = keyboard::key_event(sc, true) {
                            let _ = session::send_input(&connected.client, &ev);
                        }
                    }
                    // Magic Remote Back (0x200003): no scancode of its own — forwarded to
                    // the host as Esc. A held Back never arrives here; webOS delivers it as
                    // the EXIT gesture instead (see the `WEBOS_EXIT_SCANCODE` poll below).
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
                        // Still forwarded: every shortcut button is also game input, and
                        // the hold requirement is what keeps the two uses apart.
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
            // A disconnect chord held long enough (and the dialog isn't already up) —
            // open it, then forget the chord so it fires once per hold rather than
            // repeatedly while the buttons stay down.
            if !disconnect.is_open() && chord.held_for(EXIT_HOLD) {
                tracing::info!("disconnect shortcut held — opening dialog");
                chord.clear();
                disconnect.open(1);
            }
            // EXIT gesture (held Back) opens the dialog; a short Back tap is Esc, above.
            if exit_gesture_fired(&mut exit_held) && !disconnect.is_open() {
                tracing::info!("EXIT gesture — opening disconnect dialog");
                disconnect.open(1);
            }
            // Green button: local-only stats-overlay toggle, edge-detected here (raw
            // scancode poll — the safe SDL2 event API can't see this key at all).
            // Skipped while the disconnect dialog owns input, same as scancode forwarding.
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
            // Yellow button: log-tail overlay Off -> Live -> Frozen -> Off, same
            // edge-detect technique as Green above. Works on every screen, not just
            // while streaming — see the matching handling in `run_ui_flow`.
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
            // Blue button: live frame-pacing toggle, same edge-detect as Green/Yellow above
            // (Red is OS-intercepted on-device). Force a redraw so Pace reflects the new state.
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
            // Captured once: reused below to skip the stats overlay for exactly the
            // ticks the dialog block itself owns the canvas — that's wider than
            // `is_open()`, since a dismissed dialog still draws (fading out) for a
            // few more ticks after `focus` has already gone back to `None`.
            let dialog_frame = disconnect.frame(MODAL_FADE);
            if dialog_frame.is_some() {
                // Its own clear/present pass over the punch-through video plane, unlike
                // the menu which appends into a shared command list.
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
            // Host→client gamepad feedback: rumble onto the pad via SDL, DualSense
            // trigger/lightbar effects via the Bluetooth service. Called unconditionally
            // so both planes keep draining even with no pad attached — see the fn's docs.
            session::pump_feedback_once(&connected.client, controller.as_mut(), ds_feedback.as_mut());
            // Skipped whenever the dialog block drew this tick (open or still fading
            // out) — its own redraw above already owns the canvas. Stats and the log
            // overlay share one clear/execute/present so neither erases the other's
            // tile with its own `canvas.clear()`.
            //
            // `log_overlay_lines()` is deferred to the throttled block below rather
            // than called every ~2ms tick — it locks the same ring-buffer mutex every
            // log call writes to, which was contending with log writes ~500x/s and
            // stuttering this thread's input polling.
            let notif_frame = if dialog_frame.is_none() { notif.frame() } else { None };
            let notif_active = notif_frame.is_some();
            // Stats/log fade in on their toggle and fade out the same way the toast does
            // (same `OVERLAY_FADE` curve) instead of cutting instantly — `visibility_alpha`
            // keeps returning `Some` through the close fade even once the toggle itself
            // has already flipped off.
            let stats_alpha = stats_fade.visibility_alpha(crate::ui::OVERLAY_FADE, stats_enabled);
            let log_overlay_on = log_overlay_state() != LogOverlayState::Off;
            let log_alpha = log_fade.visibility_alpha(crate::ui::OVERLAY_FADE, log_overlay_on);
            let overlay_active = stats_alpha.is_some() || log_alpha.is_some() || notif_active;
            if overlay_was_active && !overlay_active {
                // Same "nothing else clears this canvas" wipe as before — the last
                // faded-out tile would otherwise stick over the video.
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
                // Content stays on a 500ms cadence even when the loop runs faster for a
                // toast fade; `stats_dst` lets the retained texture recomposite every frame.
                if stats_enabled && stats_built_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                    stats_built_at = Some(Instant::now());
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
                // Re-rendered only while actually on (`None` during the fade-out, once
                // the toggle has already flipped the state to Off) — the fade keeps
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
                if let Some((text, alpha)) = &notif_frame {
                    match crate::ui::render_notification_tile(fonts.raster, fonts.value, text) {
                        Ok(tile) => {
                            let (tw, th) = (tile.width(), tile.height());
                            compositor.upload(&texture_creator, Tile::Notification, &tile)?;
                            // Top-centre: clears the top-right stats overlay and the
                            // bottom log overlay, so it never overlaps either.
                            cmds.push(DrawCmd::Tex {
                                tile: Tile::Notification,
                                dst: crate::ui::render::Rect::new((display_mode.w - tw as i32) / 2, 24, tw, th),
                                alpha: (alpha * 255.0) as u8,
                            });
                        }
                        Err(e) => tracing::warn!("toast render failed: {e:#}"),
                    }
                }
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

        // Hand the pad back before anything else: trigger resistance is firmware state
        // that outlives the session, so a game that ended with R2 stiff would leave it
        // stiff on the TV's home screen (and after the app exits) with nothing to connect
        // it to punktfunk. Dropping the sender flushes and joins it — see `Feedback::drop`.
        if let Some(mut fb) = ds_feedback.take() {
            fb.release();
        }
        // Any rumble still running is likewise the pad's own state, not the stream's.
        if let Some(pad) = controller.as_mut() {
            let _ = pad.set_rumble(0, 0, 0);
        }
        // `disconnect_quit()` was already called above for every deliberate-stop path;
        // `shutdown()` joins the video thread and drops `client` so the QUIC close
        // frame actually gets sent before this function returns (see its docs). A `false`
        // return means some teardown thread is still wedged inside an FFI call — unloading
        // NDL from under it would race, so skip that call and accept the leak for this run.
        if connected.shutdown() {
            crate::platform::webos::ndl::quit();
        } else {
            tracing::warn!("session teardown timed out — skipping NDL unload for this run");
        }
        // Put the TV's picture/sound modes back (no-op unless game mode switched them).
        crate::platform::webos::game_mode::restore(restore_tv_modes);
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
