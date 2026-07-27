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
    /// Enters the "host unreachable — wake it?" flow (see `WakeState`'s docs). With
    /// this host's `KnownHost::wol_auto` off, this shows the prompt right away,
    /// replacing what used to be a plain error message. With it on, the packet fires
    /// immediately and silently, and the prompt only appears if the host still hasn't
    /// come back one `WAKE_RETRY_INTERVAL` later (`tick_wake`).
    pub(crate) fn start_wake(&mut self, host: String, port: u16, mac: Vec<String>, reason: String) {
        let known = self.known_hosts.iter().find(|h| h.host == host && h.port == port);
        let name = known.map_or_else(|| host.clone(), |h| h.name.clone());
        // Nothing to send without a MAC on record — never pretend to auto-send in
        // that case, just show the (mac-less) interactive explanation instead.
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

    /// Fires (or re-fires) the magic packet for an in-flight wake, bumping its resend
    /// timer — shared by the modal's explicit "Send" action and `tick_wake`'s periodic
    /// resend.
    pub(crate) fn send_wake(wake: &mut WakeState) {
        // Only claim "sent" if a magic packet actually went out — `wake_and_log`
        // returns false on an unparseable MAC / no usable interface, and showing
        // "Sent a wake signal… waiting" for a packet that never left would leave
        // the user waiting on nothing.
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

    /// Advances an in-flight wake: resends the WOL packet every `WAKE_RETRY_INTERVAL`
    /// (once a MAC is on record — see `WakeState::mac`'s docs), surfaces a silent
    /// auto-send as the visible prompt at that same mark, and — regardless of
    /// either — actively re-checks reachability every `WAKE_PROBE_INTERVAL` via
    /// `wake_probe`, ending the wake via `wake_succeeded` on success. This runs whether
    /// or not `Screen::Wake` is actually showing (same as the WOL timers), since a
    /// silent auto-send wait has no modal open at all; `drain_discovery`'s passive mDNS
    /// check can also end a wake independently, whichever notices first. Called every UI
    /// tick; returns whether anything visibly changed (same contract as
    /// `drain_discovery`/`drain_art`).
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

        // Only ever *resend*, gated on `wake.sent` — without it, this fired the first
        // WOL packet on the very next tick after `start_wake` regardless of the host's
        // `wol_auto` (`last_attempt: None` reads as "due"). The first send is either
        // `start_wake`'s own immediate call (auto-send on) or the user's explicit
        // Confirm on "Wake host" (`handle_wake_event`).
        let retry_due = !wake.mac.is_empty()
            && wake.sent
            && wake
                .last_attempt
                .is_some_and(|t| now.duration_since(t) >= WAKE_RETRY_INTERVAL);
        // A minute of retrying hasn't brought the host back, so a silent wait stops
        // being silent — the user should be looking at this rather than at a grid that
        // just isn't loading. Only ever once: after this the prompt is up (or was, and
        // they dismissed it), and re-popping it every minute would be nagging.
        let reveal = retry_due && wake.silent;
        if retry_due {
            Self::send_wake(wake);
            wake.silent = false;
            // Deferred rather than assigned here: `wake` still borrows `self.wake` for
            // the rest of the function.
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
        // Past `wake`'s last use, so these can take `&mut self` again.
        if reveal {
            self.screen = Screen::Wake;
        }
        // Only touched when a resend actually happened — a wake the user is watching in
        // the modal still updates the bar underneath it, which is what they'll see on Back.
        if let Some(status) = new_status {
            self.home_status = Some(status);
        }
        changed
    }

    /// Kicks off one reachability probe for `(host, port)` — the same mTLS library
    /// fetch `confirm_grid_card`'s pre-flight check uses, reused here as `tick_wake`'s
    /// active "is it back yet" signal. A plain associated function (not `&self`) so it
    /// can be called while `tick_wake` already holds `&mut self.wake`.
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

    /// Handles one menu event on the Wake modal — a Forget-host-style
    /// "Wake host"/"Cancel" button pair (focus `0`/`1`, see `render_wake`) and
    /// nothing else. Any direction key moves between the two; Confirm sends or
    /// cancels. Back dismisses the modal, same as Cancel — see `dismiss_wake` for
    /// what that leaves on the Home status bar. The auto-send toggle that used to
    /// live here is now per-host, in `Screen::WakeSettings`.
    pub fn handle_wake_event(&mut self, ev: MenuEvent) {
        let Some(wake) = self.wake.as_mut() else { return };
        // No MAC on record for this host yet — there's nothing to send or automate
        // (see `render_wake`, which hides the toggle/buttons in this case too), so
        // every event but Back (handled below, same as always) is a no-op.
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
                // focused == 1 ("Cancel") — same as Back.
                self.dismiss_wake();
            }
            MenuEvent::Back | MenuEvent::Secondary => {}
        }
    }

    /// Closes the Wake modal (Back or "Cancel"). A wake with a packet already out
    /// keeps running in the background — the resend/probe timers are what actually
    /// bring the host back, and dropping them because the user stopped watching the
    /// modal would silently abandon a wake in progress; it just goes quiet, with the
    /// Home status bar carrying the wait from here (`wake_home_status`). Nothing
    /// sent means there's nothing to keep, so that case drops the wake entirely and
    /// leaves the plain error text behind, as this always used to.
    fn dismiss_wake(&mut self) {
        self.screen = Screen::Home;
        match self.wake.as_mut() {
            Some(wake) if wake.sent => {
                // Left `false` deliberately (`tick_wake`'s escalation is gated on it):
                // the user has now seen this prompt and chosen to close it, so it must
                // not pop itself back up a minute later.
                wake.silent = false;
                self.home_status = Some(Self::wake_home_status(wake));
            }
            _ => self.home_status = self.wake.take().map(|w| w.reason),
        }
    }
    /// The wake modal's card rect — shared by `render_wake` and mouse
    /// hit-testing. Height fits `wake`'s status (one or two lines) plus the
    /// buttons, which are absent entirely with no MAC on record.
    pub(crate) fn wake_card_rect(screen_w: u32, screen_h: u32, wake: &WakeState, fonts: &ui::Fonts) -> Rect {
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            let header_end = ui::modal_header_end_y(fonts.title, fonts.label, probe, &Self::wake_status_text(wake));
            if wake.mac.is_empty() {
                (header_end + 32) as u32
            } else {
                (header_end + 28 + 72 + 32) as u32
            }
        })
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
        let card = Self::wake_card_rect(screen_w, screen_h, wake, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;

        let status = Self::wake_status_text(wake);
        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.title,
            fonts.label,
            card,
            Self::wake_title(wake),
            ui::WHITE,
            &status,
            ui::MUTED,
        )?;

        // No MAC on record — nothing to send, so there's nothing else to draw (see
        // `handle_wake_event`'s matching guard); the status text above already
        // explains why, and `App::drain_discovery` still reconnects automatically the
        // moment this host reappears on mDNS.
        if !wake.mac.is_empty() {
            // `usize::MAX` = nothing focused; the focused button is a separate
            // `Tile::ModalFocusElement` (see `prepare_tiles`).
            let buttons = Self::wake_buttons_rect(card, wake, fonts);
            ui::draw_confirm_buttons(painter, text_cache, fonts, buttons, &Self::wake_buttons(), usize::MAX)?;
        }
        Ok(())
    }

    /// The Wake/Cancel button pair — shared by `render_wake`'s shell and the
    /// focused-button tile (`prepare_tiles`), so their `ConfirmButton` data
    /// can't drift apart. Mirrors `forget_buttons`.
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

    /// The modal's title. With a MAC on record this card *offers an action*, so it asks
    /// for one; without a MAC there is nothing to offer and it can only report state.
    /// Titling both "Host unreachable" made the actionable case read as an error message.
    pub(crate) fn wake_title(wake: &WakeState) -> &'static str {
        if wake.mac.is_empty() {
            "Host unreachable"
        } else if wake.sent {
            "Waking host…"
        } else {
            "Wake this host?"
        }
    }

    /// The Home bottom-bar line for a wake that is running with no modal in front
    /// of it — an auto-send wait (`KnownHost::wol_auto`), or one the user sent
    /// and then dismissed. Separate from `wake_status_text` because that one is a
    /// modal subtitle sitting under a title that already says "Waking host…",
    /// whereas this line has to carry the whole state on its own.
    pub(crate) fn wake_home_status(wake: &WakeState) -> String {
        match wake.attempts {
            // Nothing went out — `send_wake` has already put the why in `reason`.
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

    /// The Wake modal's status line — depends on the host's name and whether a wake
    /// was just sent, so unlike Pairing's fixed subtitle this isn't a constant, but
    /// it's still reconstructible from `wake` alone. Shared by `render_wake` (drawing
    /// it) and `wake_buttons_rect` (measuring it, without drawing, to place the
    /// buttons under it).
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

    /// The Wake/Cancel button row's rect, stacked under the status text — whose
    /// wrapped height it therefore depends on, computed via `ui::modal_header_end_y`
    /// without drawing so `prepare_tiles`/`draw_list` can position the focused-button
    /// tile without re-rendering the header. Mirrors `forget_host_content_rect`'s
    /// button-row sizing.
    pub(crate) fn wake_buttons_rect(card: Rect, wake: &WakeState, fonts: &ui::Fonts) -> Rect {
        let after_status_y = ui::modal_header_end_y(fonts.title, fonts.label, card, &Self::wake_status_text(wake));
        Rect::new(card.x() + 32, after_status_y + 28, card.width().saturating_sub(64), 72)
    }
}
