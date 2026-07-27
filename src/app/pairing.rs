use super::*;
use crate::store::{self, KnownHost};
use crate::ui::{self, HostEntry, MenuEvent, Painter};
use anyhow::Result;
use sdl2::rect::Rect;
use std::time::Instant;

impl App {
    /// Open pairing modal and reset PIN state.
    pub(crate) fn open_pairing(&mut self, idx: usize) {
        self.pairing_entry = idx;
        self.pin_digits = [0; 4];
        self.pin_digit_index = 0;
        // Request access is the default: it is the path that always works, whereas the
        // PIN additionally needs the host's pairing page open and armed.
        self.pairing_focus = PairingFocus::RequestAccess;
        self.pairing_status = None;
        self.screen = Screen::Pairing;
    }

    /// Handle pairing events (PIN row or Request Access button).
    pub fn handle_pairing_event(&mut self, ev: MenuEvent) {
        if self.pairing_busy {
            // Mid-ceremony, Back cancels (dropping the receiver orphans the
            // worker — its send fails and it exits); everything else is ignored.
            if ev == MenuEvent::Back {
                self.pairing_rx = None;
                self.pairing_busy = false;
                self.pairing_status = None;
                self.screen = Screen::Home;
            }
            return;
        }
        // Back always leaves the modal; Secondary is the "switch pairing method"
        // shortcut — both work from either focus zone.
        match ev {
            MenuEvent::Back => {
                self.screen = Screen::Home;
                return;
            }
            MenuEvent::Secondary => {
                self.pairing_focus = match self.pairing_focus {
                    PairingFocus::Pin => PairingFocus::RequestAccess,
                    PairingFocus::RequestAccess => PairingFocus::Pin,
                };
                self.modal_focus_anim = Some(Instant::now());
                return;
            }
            _ => {}
        }
        match self.pairing_focus {
            // The digits sit in a horizontal row: Left/Right move *between* them and
            // Up/Down spin the focused digit's *value* (odometer-style: Up = +1, Down =
            // −1, wrapping 0..=9). Tabbing Right off the last digit drops focus onto the
            // "Request access" button below; `Confirm` submits the PIN.
            PairingFocus::Pin => match ev {
                MenuEvent::Up => {
                    self.pin_digits[self.pin_digit_index] = (self.pin_digits[self.pin_digit_index] + 1) % 10;
                }
                MenuEvent::Down => {
                    self.pin_digits[self.pin_digit_index] = (self.pin_digits[self.pin_digit_index] + 9) % 10;
                }
                MenuEvent::Left => {
                    // Off the left-hand end goes back up to the primary button, so the two
                    // options are reachable from each other without the Secondary key.
                    if self.pin_digit_index > 0 {
                        self.pin_digit_index -= 1;
                    } else {
                        self.pairing_focus = PairingFocus::RequestAccess;
                    }
                    self.modal_focus_anim = Some(Instant::now());
                }
                MenuEvent::Right => {
                    // Stops at the last digit — the button is *above* this row now, so
                    // tabbing off the right-hand end no longer corresponds to anything.
                    if self.pin_digit_index + 1 < self.pin_digits.len() {
                        self.pin_digit_index += 1;
                        self.modal_focus_anim = Some(Instant::now());
                    }
                }
                MenuEvent::Confirm => self.try_pair(),
                MenuEvent::Back | MenuEvent::Secondary => {} // handled above
            },
            // Left tabs back onto the PIN row; Confirm sends the access request.
            // Down (and Right, which reads the same way on a d-pad here) drops to the PIN
            // row below the "or" rule.
            PairingFocus::RequestAccess => match ev {
                MenuEvent::Down | MenuEvent::Right => {
                    self.pairing_focus = PairingFocus::Pin;
                    self.pin_digit_index = 0;
                    self.modal_focus_anim = Some(Instant::now());
                }
                MenuEvent::Confirm => self.try_request_access(),
                MenuEvent::Up | MenuEvent::Left | MenuEvent::Back | MenuEvent::Secondary => {}
            },
        }
    }

    /// No-PIN path: request access (park), then pin fingerprint. 185s timeout.
    pub(crate) fn try_request_access(&mut self) {
        let entry = &self.entries[self.pairing_entry];
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        let mgmt_port = entry.mgmt_port();
        let mac = entry.mac().to_vec();
        self.pairing_busy = true;
        self.pairing_status = Some("Requesting access — approve this TV on the host.".into());
        tracing::info!("requesting access to {host}:{port}");

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.pairing_rx = Some(rx);
        std::thread::spawn(move || {
            let result = crate::session::request_access(&host, port, identity, std::time::Duration::from_secs(185))
                .map_err(|e| crate::errors::friendly(&e));
            let _ = tx.send(PairingOutcome {
                host,
                port,
                name,
                mgmt_port,
                mac,
                result,
            });
        });
    }

    /// Drain finished pairing; persist on success, show error on failure.
    pub fn drain_pairing(&mut self) -> bool {
        let Some(rx) = &self.pairing_rx else { return false };
        let Ok(outcome) = rx.try_recv() else { return false };
        self.pairing_rx = None;
        self.pairing_busy = false;
        match outcome.result {
            Ok(fingerprint) => {
                tracing::info!("paired ok ({}:{}), fingerprint set", outcome.host, outcome.port);
                store::upsert_known_host(
                    &mut self.known_hosts,
                    KnownHost {
                        name: outcome.name,
                        host: outcome.host.clone(),
                        port: outcome.port,
                        fingerprint: Some(fingerprint),
                        mgmt_port: outcome.mgmt_port,
                        mac: outcome.mac,
                        // Preserved across a re-add by `upsert_known_host`; off for a genuinely new host.
                        wol_auto: false,
                        // Only reaches a genuinely new host — `upsert_known_host` keeps an
                        // existing record's pins.
                        pinned: vec![store::DESKTOP_PIN_ID.to_string()],
                    },
                );
                let _ = store::save_known_hosts(&self.known_hosts);
                self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
                self.sidebar_dirty = true;
                self.screen = Screen::Home;
                self.select_host(outcome.host, outcome.port, outcome.mgmt_port);
            }
            Err(e) => {
                tracing::warn!("pairing/request failed: {e}");
                self.pairing_status = Some(e);
            }
        }
        true
    }

    /// Number button entry; auto-advances like phone PIN pad.
    pub fn enter_pin_digit(&mut self, digit: u8) {
        if self.pairing_busy {
            return;
        }
        // A typed digit is unambiguously PIN input — pull focus back off the
        // Request-access button so it lands in the digit row (and can't
        // accidentally auto-submit the no-PIN path instead).
        self.pairing_focus = PairingFocus::Pin;
        self.pin_digits[self.pin_digit_index] = digit;
        if self.pin_digit_index + 1 < self.pin_digits.len() {
            self.pin_digit_index += 1;
        } else {
            self.try_pair();
        }
    }

    /// Start PIN pairing on background thread (30s timeout).
    pub(crate) fn try_pair(&mut self) {
        let entry = &self.entries[self.pairing_entry];
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        let mgmt_port = entry.mgmt_port();
        let mac = entry.mac().to_vec();
        let pin: String = self.pin_digits.iter().map(std::string::ToString::to_string).collect();
        self.pairing_busy = true;
        self.pairing_status = Some("Pairing — confirm the PIN on the host.".into());
        tracing::info!("pairing with {host}:{port} (pin len {})", pin.len());

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.pairing_rx = Some(rx);
        std::thread::spawn(move || {
            let result = punktfunk_core::client::NativeClient::pair(
                &host,
                port,
                (&identity.0, &identity.1),
                &pin,
                "webOS TV",
                std::time::Duration::from_secs(30),
            )
            .map_err(|e| crate::errors::pair_message(&e));
            // Send failing just means the user backed out and the receiver is
            // gone — nothing to deliver to.
            let _ = tx.send(PairingOutcome {
                host,
                port,
                name,
                mgmt_port,
                mac,
                result,
            });
        });
    }
}

/// All y-positions on pairing card, computed once (keeps renderer, hit-test, and tile prep in sync).
pub(crate) struct PairingLayout {
    pub(crate) button: Rect,
    pub(crate) button_caption_y: i32,
    pub(crate) or_y: i32,
    pub(crate) pin_caption_y: i32,
    pub(crate) pin_y: i32,
    pub(crate) status_y: i32,
    /// The card's inner column, for full-width rules and centred captions.
    pub(crate) content: Rect,
}

/// Request access button height and card side inset.
const PAIRING_BUTTON_H: u32 = 64;
const PAIRING_MARGIN: i32 = 40;

impl App {
    pub(crate) fn pairing_layout(card: Rect, fonts: &ui::Fonts) -> PairingLayout {
        let content = Rect::new(
            card.x() + PAIRING_MARGIN,
            card.y(),
            card.width().saturating_sub(PAIRING_MARGIN as u32 * 2),
            0,
        );
        let header_end = ui::modal_header_end_y(fonts.label, fonts.value, card, PAIRING_SUBTITLE);
        let button = Rect::new(content.x(), header_end + 26, content.width(), PAIRING_BUTTON_H);
        let button_caption_y = button.y() + button.height() as i32 + 12;
        let or_y = button_caption_y + fonts.value.height() + 20;
        let pin_caption_y = or_y + fonts.value.height() + 20;
        let pin_y = pin_caption_y + fonts.value.height() + 14;
        let status_y = pin_y + ui::PAIRING_DIGIT_H as i32 + 22;
        PairingLayout {
            button,
            button_caption_y,
            or_y,
            pin_caption_y,
            pin_y,
            status_y,
            content,
        }
    }

    /// Card rect, sized from layout plus room for up-to-two-line status.
    pub(crate) fn pairing_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            let l = Self::pairing_layout(probe, fonts);
            let status_room = 2 * (fonts.value.height() + 6);
            (l.status_y + status_room + 26) as u32
        })
    }

    /// Request access button rect.
    pub(crate) fn pairing_request_button_rect(card: Rect, fonts: &ui::Fonts) -> Rect {
        Self::pairing_layout(card, fonts).button
    }

    /// PIN row top y-position.
    pub(crate) fn pairing_pin_row_y(card: Rect, fonts: &ui::Fonts) -> i32 {
        Self::pairing_layout(card, fonts).pin_y
    }

    pub(crate) fn render_pairing(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = Self::pairing_card_rect(screen_w, screen_h, fonts);
        let l = Self::pairing_layout(card, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;

        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.label,
            fonts.value,
            card,
            "Pair with host",
            ui::WHITE,
            PAIRING_SUBTITLE,
            ui::MUTED,
        )?;

        // Primary first, and visually primary: approving on the host is the path that
        // always works, whereas the PIN needs the host's pairing page open and armed.
        // The shell draws it unfocused-but-filled; the focused copy is a separate
        // `Tile::ModalFocusElement` (see `prepare_tiles`).
        ui::draw_primary_button(painter, text_cache, fonts.label, l.button, ui::PAIRING_REQUEST_LABEL)?;
        Self::draw_centred_caption(
            painter,
            text_cache,
            fonts.value,
            l.content,
            l.button_caption_y,
            "Then approve this TV on the host.",
        )?;

        ui::draw_or_divider(painter, text_cache, fonts.value, l.content, l.or_y, "or")?;

        Self::draw_centred_caption(
            painter,
            text_cache,
            fonts.value,
            l.content,
            l.pin_caption_y,
            "Enter the PIN shown on the host.",
        )?;
        for (i, digit) in self.pin_digits.iter().enumerate() {
            let rect = ui::pairing_digit_rect(card, l.pin_y, i);
            let drawn = ui::draw_card(painter, rect, false);
            let text = digit.to_string();
            let tw = fonts.title.size_of(&text).map_or(0, |(w, _)| w);
            ui::draw_text(
                painter,
                text_cache,
                fonts.title,
                &text,
                drawn.x() + (drawn.width() as i32 - tw as i32) / 2,
                drawn.y() + (drawn.height() as i32 - fonts.title.height()) / 2,
                ui::WHITE,
            )?;
        }

        if let Some(status) = &self.pairing_status {
            let color = if self.pairing_busy { ui::MUTED } else { ui::ERROR_RED };
            ui::draw_text_wrapped(
                painter,
                text_cache,
                fonts.value,
                status,
                l.content.x(),
                l.status_y,
                l.content.width(),
                color,
                6,
            )?;
        }
        Ok(())
    }

    /// Centred caption line (option labels on either side of "or" rule).
    fn draw_centred_caption(
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        font: &sdl2::ttf::Font,
        content: Rect,
        y: i32,
        text: &str,
    ) -> Result<()> {
        let w = font.size_of(text).map_or(0, |(w, _)| w) as i32;
        ui::draw_text(
            painter,
            text_cache,
            font,
            text,
            content.x() + (content.width() as i32 - w) / 2,
            y,
            ui::MUTED,
        )?;
        Ok(())
    }
}
