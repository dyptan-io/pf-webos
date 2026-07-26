//! The manual add-host-by-IP modal.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
use super::*;
use crate::store::{self, KnownHost};
use crate::ui::{self, HostEntry, MenuEvent, Painter};
use anyhow::Result;
use sdl2::rect::Rect;

/// The add-host screen's subtitle. `Screen::EditHost` builds its own — see
/// `App::address_subtitle`.
const ADD_HOST_SUBTITLE: &str = "Enter the host's IP address.";

impl App {
    /// Handles one menu event on the manual add-host modal — a plain, growing
    /// IP digit string with no port field (see `ui::AddHostState`'s docs).
    /// Left/Right stand in for backspace/"next octet" (no dot key on the
    /// remote); Confirm submits once four octets have been typed.
    pub fn handle_add_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.add_host.backspace(),
            MenuEvent::Right => self.add_host.advance_octet(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
            MenuEvent::Confirm => self.confirm_add_host(),
            MenuEvent::Back => self.screen = Screen::Home,
        }
    }

    /// Direct digit entry (the Magic Remote's number buttons) on the add-host
    /// modal — same auto-advance idiom as `enter_pin_digit`.
    pub fn enter_add_host_digit(&mut self, digit: u8) {
        self.add_host.enter_digit(digit);
    }

    /// No-op until all four octets have been typed (`ui::AddHostState::is_complete`)
    /// — Confirm on a still-partial address just does nothing rather than
    /// connecting to a truncated/zero-padded guess.
    pub(crate) fn confirm_add_host(&mut self) {
        if !self.add_host.is_complete() {
            return;
        }
        let (host, port) = self.add_host.host_and_port();
        store::upsert_known_host(
            &mut self.known_hosts,
            KnownHost {
                name: host.clone(),
                host: host.clone(),
                port,
                fingerprint: None,
                mgmt_port: None,
                mac: Vec::new(),
            },
        );
        let _ = store::save_known_hosts(&self.known_hosts);
        self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();
        self.home_focus = HomeFocus::Sidebar(
            self.entries
                .iter()
                .position(|e| e.host() == host && e.port() == port)
                .unwrap_or(0),
        );
        self.screen = Screen::Home;
    }
    /// One character from the webOS on-screen keyboard (`Event::TextInput`). Shared by
    /// `Screen::AddHost` and `Screen::EditHost`, which edit the same `AddHostState`.
    pub(crate) fn enter_host_address_char(&mut self, c: char) {
        self.add_host.enter_char(c);
    }

    /// The address form's subtitle for whichever screen is open.
    ///
    /// This drives the card's *height*, so it can't be a fixed literal: `Screen::AddHost`
    /// has a one-line subtitle, but `Screen::EditHost`'s carries the host's name and wraps
    /// to two for a long one. Sizing both cards from the Add-host string pushed the input
    /// field out through the bottom of the card whenever that happened.
    pub(crate) fn address_subtitle(&self) -> String {
        match self.screen {
            Screen::EditHost => {
                let name = self
                    .edit_host_index
                    .and_then(|i| self.entries.get(i))
                    .map_or_else(String::new, |e| e.name().to_string());
                format!("New IP address for {name}. Its pairing is kept.")
            }
            _ => ADD_HOST_SUBTITLE.to_string(),
        }
    }

    /// The address field's on-screen rect, also handed to `SDL_SetTextInputRect`.
    ///
    /// Setting that rect is the correct contract — it tells the platform which region the
    /// panel should avoid — but webOS's OSK does not honour it here, which is why the card
    /// is anchored high instead (see `App::keyboard_modal_card`). Kept because it costs
    /// nothing and is right if the fork ever starts respecting it.
    pub(crate) fn address_field_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        let subtitle = self.address_subtitle();
        let card = self.address_card_rect(screen_w, screen_h, fonts);
        let after_subtitle_y = ui::modal_header_end_y(fonts.label, fonts.value, card, &subtitle);
        Rect::new(
            card.x() + 32,
            after_subtitle_y + 20,
            card.width().saturating_sub(64),
            80,
        )
    }

    /// The address form's card rect — shared by the renderer and mouse hit-testing.
    ///
    /// Lifts clear of webOS's on-screen keyboard while that's up, and sits where any other
    /// modal sits when it isn't — see `App::keyboard_modal_card`.
    pub(crate) fn address_card_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        let subtitle = self.address_subtitle();
        self.keyboard_modal_card(screen_w, screen_h, |probe| {
            let header_end = ui::modal_header_end_y(fonts.label, fonts.value, probe, &subtitle);
            (header_end + 20 + 80 + 32) as u32 // field + bottom margin
        })
    }

    pub(crate) fn render_add_host(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        self.render_host_address_form(painter, text_cache, fonts, screen_w, screen_h, "Add host")
    }

    /// The shared IP-entry form — one card, a header, and the typed-address field with
    /// its caret. `Screen::AddHost` and `Screen::EditHost` differ only in their title (the
    /// subtitle comes from `address_subtitle`), so they share this rather than keeping two
    /// copies that drift.
    pub(crate) fn render_host_address_form(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
        title: &str,
    ) -> Result<()> {
        let subtitle = self.address_subtitle();
        let card = self.address_card_rect(screen_w, screen_h, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;

        let after_subtitle_y = ui::draw_modal_header(
            painter,
            text_cache,
            fonts.label,
            fonts.value,
            card,
            title,
            ui::WHITE,
            &subtitle,
            ui::MUTED,
        )?;

        let field = Rect::new(
            card.x() + 32,
            after_subtitle_y + 20,
            card.width().saturating_sub(64),
            80,
        );
        let drawn = ui::draw_card(painter, field, true);
        let text_x = drawn.x() + 24;
        let typed = self.add_host.display_text();
        let text_w = fonts.title.size_of(&typed).map_or(0, |(w, _)| w);
        ui::draw_text(
            painter,
            text_cache,
            fonts.title,
            &typed,
            text_x,
            drawn.y() + (drawn.height() as i32 - fonts.title.height()) / 2,
            ui::WHITE,
        )?;
        // A blinkless text-cursor bar right after what's typed so far — there's
        // no fixed-width mask anymore to show *where* editing happens, so this
        // stands in for it.
        let caret = Rect::new(
            text_x + text_w as i32 + 6,
            drawn.y() + 16,
            3,
            drawn.height().saturating_sub(32),
        );
        painter.fill_rect(caret, ui::ACCENT_BRIGHT);
        Ok(())
    }
}
