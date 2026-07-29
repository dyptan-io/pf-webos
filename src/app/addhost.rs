use super::*;
use crate::store::{self, KnownHost};
use crate::ui::{self, HostEntry, MenuEvent, Painter};
use anyhow::Result;
use sdl2::rect::Rect;

const ADD_HOST_SUBTITLE: &str = "Enter the host's IP address.";

impl App {
    /// Handles menu event on add-host modal. Left/Right stand in for backspace (no dot
    /// key on remote); Confirm once four octets typed.
    pub fn handle_add_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.add_host.backspace(),
            MenuEvent::Right => self.add_host.advance_octet(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
            MenuEvent::Confirm => self.confirm_add_host(),
            MenuEvent::Back => self.screen = Screen::Home,
        }
    }

    /// Direct digit entry from Magic Remote number buttons.
    pub fn enter_add_host_digit(&mut self, digit: u8) {
        self.add_host.enter_digit(digit);
    }

    /// No-op until all four octets typed; prevents truncated connections.
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
                // upsert_known_host keeps an existing record's wol_auto
                wol_auto: false,
                // upsert_known_host keeps an existing record's pins
                pinned: vec![store::DESKTOP_PIN_ID.to_string()],
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
    /// Shared by `AddHost` and `EditHost`.
    pub(crate) fn enter_host_address_char(&mut self, c: char) {
        self.add_host.enter_char(c);
    }

    /// Screen-specific subtitle to avoid layout overflow.
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

    /// For `SDL_SetTextInputRect` (webOS OSK ignores it).
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

    /// Lifts clear of OSK via `keyboard_modal_card`.
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

    /// Shared by `AddHost` and `EditHost`.
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
