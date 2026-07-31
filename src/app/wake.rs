//! The "host unreachable — wake it?" flow: the Wake-on-LAN prompt, its retry/probe
//! timers, and its modal. The per-host auto-send setting the prompt obeys lives in
//! `app::wakesettings`.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
use super::*;
use crate::store::KnownHost;
use crate::ui::{self, MenuEvent, Painter};
use anyhow::Result;
use sdl2::rect::Rect;
use std::time::Instant;

impl App {
    /// Enters the WOL flow. With `wol_auto` off, shows prompt immediately.
    /// With it on, fires packet silently, shows prompt only after `WAKE_RETRY_INTERVAL`.
    pub(crate) fn start_wake(&mut self, host: String, port: u16, mac: Vec<String>, reason: String) {
        let known = self.known_hosts.iter().find(|h| h.host == host && h.port == port);
        let name = known.map_or_else(|| host.clone(), |h| h.name.clone());
        // WHY: without a MAC, don't auto-send — show interactive explanation instead.
        let auto = known.is_some_and(|h| h.wol_auto) && !mac.is_empty();
        let mut wake = WakeState {
            host,
            port,
            name,
            mac,
            reason,
            // Lands on the "Wake host" button — the reason the user is here.
            focused: 0,
            sent: false,
            attempts: 0,
            since: None,
            last_attempt: None,
            silent: auto,
            // Baseline for `WAKE_PROBE_INTERVAL` — the first active probe fires
            // `WAKE_PROBE_INTERVAL` from now, not immediately.
            last_probe: Some(Instant::now()),
            probe_rx: None,
        };
        if auto {
            Self::send_wake(&mut wake);
            // No modal is up in this branch, so the Home bar is the only place the
            // wait is visible at all — without this it would sit on `select_host`'s
            // stale "Loading library…" until the host came back (or didn't).
            self.home_status = Some(Self::wake_home_status(&wake));
        } else {
            self.screen = Screen::Wake;
        }
        self.wake = Some(wake);
    }

    /// Sends (or resends) the WOL magic packet, bumping the resend timer.
    pub(crate) fn send_wake(wake: &mut WakeState) {
        // WHY: only mark sent=true if packet actually went out; wake_and_log fails on
        // unparseable MAC or no interface. Avoid showing "Waiting…" for no packet.
        let sent = crate::wol::wake_and_log(&wake.mac, wake.host.parse().ok(), &wake.name);
        let now = Instant::now();
        if sent {
            wake.sent = true;
            wake.attempts += 1;
            wake.since.get_or_insert(now);
        } else {
            wake.reason = "Couldn't send the wake signal — no usable MAC address or network interface.".into();
        }
        wake.last_attempt = Some(now);
    }

    /// Advances an in-flight wake: resends WOL every `WAKE_RETRY_INTERVAL`, shows
    /// silent auto-send after that, and probes reachability every `WAKE_PROBE_INTERVAL`.
    /// Runs whether modal is showing or not; `drain_discovery` can also end wake.
    pub fn tick_wake(&mut self) -> bool {
        let Some(wake) = &mut self.wake else { return false };
        let now = Instant::now();
        let mut changed = false;
        let mut new_status = None;

        if let Some(rx) = &wake.probe_rx {
            if let Ok(loaded) = rx.try_recv() {
                wake.probe_rx = None;
                changed = true;
                if loaded.result.is_ok() {
                    let (host, port) = (wake.host.clone(), wake.port);
                    let mgmt_port = self
                        .known_hosts
                        .iter()
                        .find(|h| h.host == host && h.port == port)
                        .and_then(|h| h.mgmt_port);
                    self.wake_succeeded(host, port, mgmt_port, "reachability probe");
                    return true;
                }
                wake.last_probe = Some(now);
            }
        }
        let Some(wake) = &mut self.wake else { return changed };

        // WHY: resend only if wake.sent=true; else retry would fire on first tick
        // before user confirms. First send is start_wake's call (auto) or user's confirm.
        let retry_due = !wake.mac.is_empty()
            && wake.sent
            && wake
                .last_attempt
                .is_some_and(|t| now.duration_since(t) >= WAKE_RETRY_INTERVAL);
        // After retry_due, reveal silent wait so user sees it. Only once — re-popping
        // every minute would be nagging.
        let reveal = retry_due && wake.silent;
        if retry_due {
            Self::send_wake(wake);
            wake.silent = false;
            new_status = Some(Self::wake_home_status(wake));
            changed = true;
        }

        if wake.probe_rx.is_none()
            && wake
                .last_probe
                .is_some_and(|t| now.duration_since(t) >= WAKE_PROBE_INTERVAL)
        {
            let (host, port) = (wake.host.clone(), wake.port);
            wake.probe_rx = Some(Self::wake_probe(&self.known_hosts, &self.identity, &host, port));
            wake.last_probe = Some(now);
        }
        if reveal {
            self.screen = Screen::Wake;
        }
        if let Some(status) = new_status {
            self.home_status = Some(status);
        }
        changed
    }

    /// Spawns a reachability probe for (host, port). Associated function (not &self)
    /// so it can run while `tick_wake` holds &mut self.wake.
    pub(crate) fn wake_probe(
        known_hosts: &[KnownHost],
        identity: &(String, String),
        host: &str,
        port: u16,
    ) -> std::sync::mpsc::Receiver<crate::library::GamesLoaded> {
        let known = known_hosts.iter().find(|h| h.host == host && h.port == port);
        let mgmt_port = known
            .and_then(|h| h.mgmt_port)
            .unwrap_or(crate::library::DEFAULT_MGMT_PORT);
        let fingerprint = known.and_then(|h| h.fingerprint);
        crate::library::load_games_async(host.to_string(), port, mgmt_port, identity.clone(), fingerprint)
    }

    /// Handles Wake modal events: direction moves between "Wake"/"Cancel" buttons.
    /// Confirm sends or cancels. Back dismisses the modal (keeps wake running in bg).
    pub fn handle_wake_event(&mut self, ev: MenuEvent) {
        let Some(wake) = self.wake.as_mut() else { return };
        // WHY: no MAC = no send/automate possible. Every event but Back is no-op.
        if wake.mac.is_empty() && ev != MenuEvent::Back {
            return;
        }
        if ev == MenuEvent::Back {
            self.dismiss_wake();
            return;
        }
        match ev {
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right => {
                wake.focused = usize::from(wake.focused == 0);
                self.modal_focus_anim = Some(Instant::now());
            }
            MenuEvent::Confirm if wake.focused == 0 => Self::send_wake(wake),
            MenuEvent::Confirm => {
                self.dismiss_wake();
            }
            MenuEvent::Back | MenuEvent::Secondary => {}
        }
    }

    /// Closes Wake modal. Sent wakes keep running in background (timers bring host back).
    /// Unsent wakes drop entirely, leaving error text behind.
    fn dismiss_wake(&mut self) {
        self.screen = Screen::Home;
        match self.wake.as_mut() {
            Some(wake) if wake.sent => {
                // WHY: set silent=false so tick_wake won't re-pop the prompt after user dismisses.
                wake.silent = false;
                self.home_status = Some(Self::wake_home_status(wake));
            }
            _ => self.home_status = self.wake.take().map(|w| w.reason),
        }
    }
    /// Card for the no-MAC modal: an informational "Host unreachable" message with no
    /// button row (nothing to send), so it's a plain message card, not a confirm dialog.
    pub(crate) fn wake_message_card(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, status: &str) -> Rect {
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            (ui::modal_header_end_y(fonts.label, fonts.value, probe, status) + 32) as u32
        })
    }

    /// Wake modal card rect. With a MAC it's the shared confirmation dialog; without one
    /// it's the button-less informational card.
    pub(crate) fn wake_card_rect(screen_w: u32, screen_h: u32, wake: &WakeState, fonts: &ui::Fonts) -> Rect {
        let status = Self::wake_status_text(wake);
        if wake.mac.is_empty() {
            Self::wake_message_card(screen_w, screen_h, fonts, &status)
        } else {
            ui::confirm_dialog_card(screen_w, screen_h, fonts, &status)
        }
    }

    pub(crate) fn render_wake(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let Some(wake) = &self.wake else { return Ok(()) };
        let status = Self::wake_status_text(wake);

        // With a MAC it's the shared confirmation dialog (card + Wake/Cancel row); without
        // one it's a button-less informational card ("unreachable", status explains why —
        // drain_discovery reconnects automatically once the host reappears on mDNS).
        let (card, buttons) = if wake.mac.is_empty() {
            (Self::wake_message_card(screen_w, screen_h, fonts, &status), None)
        } else {
            let (card, content) = ui::confirm_dialog_layout(screen_w, screen_h, fonts, &status);
            (card, Some(content))
        };

        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        ui::draw_modal_header(
            painter, text_cache, fonts.label, fonts.value, card, Self::wake_title(wake), ui::WHITE, &status, ui::MUTED,
        )?;
        if let Some(content) = buttons {
            // usize::MAX = no focus; focused button is a separate ModalFocusElement.
            ui::draw_confirm_buttons(painter, text_cache, fonts, content, &Self::wake_buttons(), usize::MAX)?;
        }
        Ok(())
    }

    /// Wake/Cancel button pair for `render_wake` and focused-button tile.
    pub(crate) fn wake_buttons() -> [ui::ConfirmButton<'static>; 2] {
        [
            ui::ConfirmButton {
                icon: Some(ui::ICON_POWER),
                label: "Wake host",
                color: ui::ACCENT_BRIGHT,
            },
            ui::ConfirmButton {
                icon: None,
                label: "Cancel",
                color: ui::WHITE,
            },
        ]
    }

    /// Modal title varies: with MAC it's an action ("Wake this host?"), without it's state.
    pub(crate) fn wake_title(wake: &WakeState) -> &'static str {
        if wake.mac.is_empty() {
            "Host unreachable"
        } else if wake.sent {
            "Waking host…"
        } else {
            "Wake this host?"
        }
    }

    /// Home status bar line for background wake (auto-send or dismissed modal).
    /// Must stand alone; modal version sits under "Waking host…" title.
    pub(crate) fn wake_home_status(wake: &WakeState) -> String {
        match wake.attempts {
            0 => wake.reason.clone(),
            1 => format!(
                "Wake signal sent to {} — waiting for it to come back online…",
                wake.name
            ),
            n => format!(
                "Wake signal re-sent to {} ({n} attempts) — still waiting for it to come back online…",
                wake.name
            ),
        }
    }

    /// Wake modal status line; reconstructible from wake alone (used by render and layout).
    pub(crate) fn wake_status_text(wake: &WakeState) -> String {
        if wake.mac.is_empty() {
            format!(
                "{} isn't responding, and no Wake-on-LAN address is on record for it yet, so it \
                 can't be woken from here. It will reconnect automatically once it's back online.",
                wake.name
            )
        } else if wake.sent {
            format!("Wake signal sent to {}. Waiting for it to come back online…", wake.name)
        } else {
            format!("{} isn't responding. It may be powered off or asleep.", wake.name)
        }
    }
}
