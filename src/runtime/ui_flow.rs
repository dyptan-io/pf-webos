use super::*;

/// Runs the UI (host list -> pairing -> settings) until the user confirms a
/// connect target or the system asks the app to close (`None`). A plain
/// function, not a closure — a closure capturing `canvas`/`events` by
/// reference would hold that borrow for as long as the closure value exists,
/// which conflicts with using them again in the streaming loop right after.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_ui_flow(
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
    initial_toast: Option<String>,
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
        let settings = resolve_gamepad_type(store::load_settings(), game_controller);
        let handle = spawn_connect(identity.clone(), host, port, None, None, settings)?;
        return Ok(Some((handle, settings)));
    }

    canvas.window_mut().show();
    let mut app = App::new(identity.clone());
    // The GPU tile cache is the render loop's, not App's — App holds only screen state
    // (see docs/REMAINING_IMPROVEMENTS.md A2). Recreated per menu entry, same as `app`.
    let mut tiles = crate::ui::TileCache::new();
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
    // Same toast widget as the streaming loop's frame-pacer toggle (`ui::Notification`);
    // shown once, right as the Home screen re-appears.
    let mut notif = crate::ui::Notification::new();
    // Last (text, w, h) uploaded for the toast tile — see `push_notification_cmd`.
    let mut notif_tile: Option<(String, u32, u32)> = None;
    if let Some(msg) = initial_toast {
        notif.show(msg);
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
    // When the connect thread was first seen finished — the hero linger runs off this.
    let mut connect_done_at: Option<Instant> = None;
    // Yellow button log overlay works here too (see streaming loop).
    let mut yellow_held = false;
    let mut home_held = false;
    let mut log_overlay_last: Option<Instant> = None;
    // Cache last overlay tile size for idle frames (no re-render if size stable).
    let mut log_overlay_dims: Option<(u32, u32)> = None;
    // `quit_dialog_was_active` catches the close-fade's final frame so it gets one last
    // redraw-on-change tick to wipe the dialog off the menu.
    let mut quit_dialog = ConfirmDialog::new(
        "Quit app?",
        "Punktfunk will close and you'll return to the webOS home screen.",
        crate::ui::confirm_buttons(Some(crate::ui::ICON_CLOSE), "Quit", crate::ui::ERROR_RED),
    );
    let mut exit_held = false;
    // Controller routes to the quit dialog the same way it routes to the disconnect
    // dialog while streaming — see `DisconnectChord`.
    let mut chord = DisconnectChord::default();
    let mut quit_dialog_was_active = false;
    // One-shot: warm the modal text/shadow/freetype caches on the first idle tick
    // (Home already painted by then) so the first Settings/host-menu open doesn't
    // hitch on cold rasterization. Reset per menu entry — `text_cache` is too.
    let mut prewarmed = false;
    'ui: loop {
        let tick_start = Instant::now();
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            tracing::warn!("SIGTERM/SIGINT received during UI");
            return Ok(None);
        }
        // Raw scancode poll (not SDL2 event); edge-detected like streaming loop.
        let yellow_down =
            crate::platform::webos::input::webos_scancode_down(crate::platform::webos::input::WEBOS_YELLOW_SCANCODE);
        if yellow_down && !yellow_held {
            cycle_log_overlay();
            dirty = true; // force an immediate redraw with the new state
            log_overlay_last = None;
        }
        yellow_held = yellow_down;
        // Home key re-opens the webOS launcher (captured so a keyboard's Super key can
        // reach the host); works from any menu state, a long Back never trips it.
        if home_key_fired(&mut home_held) {
            crate::platform::webos::luna::launch_home();
        }
        // Long-press Back (root/held Back, surfaced as the EXIT gesture) quits
        // straight away from any menu state, no confirm — the short Back tap below
        // opens the dialog instead. Menu loop only; stream is unaffected.
        if exit_gesture_fired(&mut exit_held) {
            tracing::info!("EXIT gesture — quitting app");
            return Ok(None);
        }
        // Controller quit shortcut: held long enough on Home,
        // then forgotten so it fires once per hold rather than repeatedly while held.
        if !quit_dialog.is_open() && matches!(app.screen, Screen::Home) && chord.held_for(EXIT_HOLD) {
            tracing::info!("quit shortcut held — opening quit dialog");
            chord.clear();
            quit_dialog.open(1);
            dirty = true;
        }
        dirty |= app.drain_discovery();
        dirty |= app.drain_art();
        dirty |= app.drain_games();
        dirty |= app.drain_pairing();
        dirty |= app.drain_speed_test();
        dirty |= app.drain_send_logs();
        app.tick_reachability();
        dirty |= app.drain_reachability();
        dirty |= app.tick_wake();
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
                // In-memory settings, not `store::load_settings()`: a just-flipped
                // toggle (e.g. video pacing) is persisted asynchronously by
                // `SettingsWriter`, so re-reading disk here could race the write and
                // connect with the stale value. `app.settings` is updated synchronously.
                let settings = resolve_gamepad_type(app.settings, game_controller);
                let handle = spawn_connect(
                    identity.clone(),
                    target.host,
                    target.port,
                    Some(target.fingerprint),
                    target.launch,
                    settings,
                )?;
                connect_handle = Some((handle, settings));
            }
        }
        // Without a hero the screen is handed to the streaming loop at the end of the
        // launch fade, as it always was. With one, this loop keeps animating it as the
        // loading screen until `Hero::handover_ready` is satisfied — the handshake having
        // landed (`run_inner`'s join then returns immediately), the decoder presenting (so
        // its reveal wait is satisfied too), and the hold and fade-out done.
        if let Some(t) = app.launch_anim {
            if connect_done_at.is_none() && connect_handle.as_ref().is_none_or(|(h, _)| h.is_finished()) {
                connect_done_at = Some(Instant::now());
            }
            let presenting = crate::platform::webos::ndl::playing();
            if app.hero.handover_ready(t.elapsed(), connect_done_at, presenting) {
                break 'ui;
            }
        }
        for event in events.poll_iter() {
            use sdl2::event::Event;
            // Launch committed: the menu is behind the loading screen and its input would
            // move a grid the user can no longer see. Only shutdown still counts.
            if app.launch_anim.is_some() {
                if matches!(event, Event::Quit { .. }) {
                    tracing::info!("quit during launch");
                    return Ok(None);
                }
                continue;
            }
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
                    // An unplugged pad sends no releases — drop any armed chord.
                    chord.clear();
                    continue;
                }
                _ => {}
            }
            // Track chord state for the quit shortcut without consuming the event — the
            // buttons still flow through `handle_ui_event` for normal menu navigation.
            match event {
                Event::ControllerButtonDown { button, .. } => chord.set(button, true),
                Event::ControllerButtonUp { button, .. } => chord.set(button, false),
                _ => {}
            }
            // The quit dialog owns input while open — navigate it only, don't let the
            // event reach the menu underneath (same split as the streaming loop).
            if quit_dialog.is_open() {
                match quit_dialog.handle_event(&event, display_mode.w as u32, display_mode.h as u32, fonts) {
                    Some(ConfirmAction::Confirmed) => {
                        tracing::info!("quit confirmed from menu");
                        return Ok(None);
                    }
                    Some(_) => dirty = true,
                    None => {}
                }
                continue;
            }
            // Short Back tap on Home with sidebar focus opens the quit dialog. From a
            // game card / the ⋯ column, Back first steps focus back to the sidebar
            // (see `App::back`), so it falls through to normal dispatch instead.
            if matches!(app.screen, Screen::Home)
                && matches!(app.home_focus, HomeFocus::Sidebar(_))
                && matches!(&event, Event::KeyDown { keycode: Some(k), repeat: false, .. }
                    if crate::platform::webos::input::menu_event_for_key(*k) == Some(MenuEvent::Back))
            {
                tracing::info!("Back tap on Home sidebar — opening quit dialog");
                quit_dialog.open(1);
                dirty = true;
                continue;
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
                let r = app.address_field_rect(display_mode.w as u32, display_mode.h as u32, fonts);
                text_input.set_rect(sdl2::rect::Rect::new(r.x(), r.y(), r.width(), r.height()));
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
        // The quit dialog runs its own open/close fade and focus-pop, so keep ticking
        // while it (or its close-fade) is on screen, and force one redraw on the frame it
        // finally clears so it doesn't linger over the menu.
        let quit_dialog_active = quit_dialog.frame(MODAL_FADE).is_some();
        if quit_dialog_was_active && !quit_dialog_active {
            dirty = true;
        }
        quit_dialog_was_active = quit_dialog_active;
        // Polled every tick like the streaming loop's toast, not gated behind
        // `content_dirty` — its own fade needs frames regardless of anything else.
        let notif_frame = notif.frame();
        let animating = app.tick_animations()
            || app.tiles_pending
            || !app.grid_reveal_ready
            || quit_dialog_active
            || notif_frame.is_some();
        let log_overlay_due = log_overlay_state() != LogOverlayState::Off
            && log_overlay_last.is_none_or(|t| t.elapsed() >= Duration::from_millis(500));
        if !dirty && !animating && !log_overlay_due {
            if !prewarmed {
                prewarmed = true;
                app.prewarm_modal_caches(&mut text_cache, fonts, display_mode.w as u32, display_mode.h as u32)?;
            }
            let elapsed = tick_start.elapsed();
            if elapsed < TICK_BUDGET {
                std::thread::sleep(TICK_BUDGET - elapsed);
            }
            continue;
        }
        let content_dirty = dirty;
        dirty = false;
        // Advance per-tick app state (card size, modal fades) exactly once before compose.
        let screen_changed = app.advance_frame(display_mode.w as u32);
        let updated = app.prepare_tiles(
            &mut tiles,
            &mut text_cache,
            fonts,
            display_mode.w as u32,
            display_mode.h as u32,
            content_dirty,
            screen_changed,
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
                Tile::Hero(id) => {
                    if let Some(hero) = app.hero.image_for(id) {
                        compositor.upload_raw(texture_creator, tile.clone(), hero.width, hero.height, &hero.pixels)?;
                    }
                }
                _ => {
                    if let Some(pm) = app.tile_pixmap(&tiles, &tile) {
                        compositor.upload(texture_creator, tile, pm)?;
                    }
                }
            }
        }
        let mut cmds = app.draw_list(&tiles, display_mode.w as u32, display_mode.h as u32, fonts);
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
                match crate::ui::render_log_overlay_tile(fonts.raster, fonts.caption, display_mode.w as u32, &lines) {
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
                    dst: crate::ui::render::Rect::new(0, display_mode.h - th as i32, tw, th),
                    alpha: 0xff,
                });
            }
        }
        push_notification_cmd(
            compositor,
            texture_creator,
            fonts,
            &notif_frame,
            display_mode.w,
            &mut notif_tile,
            &mut cmds,
        )?;
        // Quit dialog overlay, appended to this loop's single command list rather than
        // getting its own present (unlike the stream, which draws over the video plane).
        quit_dialog.draw(
            compositor,
            texture_creator,
            fonts,
            display_mode.w as u32,
            display_mode.h as u32,
            &mut cmds,
        )?;
        canvas.set_blend_mode(sdl2::render::BlendMode::None);
        let bg = crate::ui::BG;
        canvas.set_draw_color(sdl2::pixels::Color::RGBA(bg.r, bg.g, bg.b, bg.a));
        canvas.clear();
        compositor.present(canvas, &cmds)?;
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
