//! Editing a saved host's address (reuses add-host widget). Fingerprint survives address
//! changes unchanged since it identifies the certificate, not the network location.
use super::*;
use sdl2::rect::Rect;

use crate::store;
use crate::ui::{self, AddHostState, HostEntry, MenuEvent, Painter};

impl App {
    /// Open `EditHost` for sidebar row; pre-filled with current address. No-op for unsaved entries.
    pub(crate) fn open_edit_host(&mut self, idx: usize) {
        let Some(HostEntry::Known(h)) = self.entries.get(idx) else {
            return;
        };
        self.add_host = AddHostState::from_ip(&h.host);
        self.edit_host_index = Some(idx);
        self.host_menu_index = None;
        self.screen = Screen::EditHost;
    }

    /// Handle menu event. Left/Right stand in for backspace; Confirm commits with 4 octets.
    pub(crate) fn handle_edit_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.add_host.backspace(),
            MenuEvent::Right => self.add_host.advance_octet(),
            MenuEvent::Confirm => self.confirm_edit_host(),
            MenuEvent::Back => {
                self.edit_host_index = None;
                self.screen = Screen::Home;
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Rewrite address in-place, keeping identity (fingerprint, `mgmt_port`, MAC). No-op if partial.
    pub(crate) fn confirm_edit_host(&mut self) {
        if !self.add_host.is_complete() {
            return;
        }
        let Some(idx) = self.edit_host_index else { return };
        let Some(HostEntry::Known(old)) = self.entries.get(idx).cloned() else {
            return;
        };
        let (host, port) = self.add_host.host_and_port();
        if host == old.host && port == old.port {
            self.edit_host_index = None;
            self.screen = Screen::Home;
            return;
        }

        // Drop old record before upsert to avoid stale entry (upsert_known_host keys on (host, port))
        self.known_hosts.retain(|k| !(k.host == old.host && k.port == old.port));
        store::upsert_known_host(
            &mut self.known_hosts,
            store::KnownHost {
                name: old.name.clone(),
                host: host.clone(),
                port,
                fingerprint: old.fingerprint,
                mgmt_port: old.mgmt_port,
                mac: old.mac.clone(),
                wol_auto: old.wol_auto,
                pinned: old.pinned.clone(),
            },
        );
        let _ = store::save_known_hosts(&self.known_hosts);
        self.entries = self.known_hosts.iter().cloned().map(HostEntry::Known).collect();

        // Keep selection updated to new address
        if self.selected_host.as_ref() == Some(&(old.host.clone(), old.port)) {
            self.selected_host = Some((host.clone(), port));
        }
        self.home_focus = HomeFocus::Sidebar(
            self.entries
                .iter()
                .position(|e| e.host() == host && e.port() == port)
                .unwrap_or(0),
        );
        self.edit_host_index = None;
        self.sidebar_dirty = true;
        self.grid_dirty = true;
        self.screen = Screen::Home;
    }

    pub(crate) fn edit_host_card_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        self.address_card_rect(screen_w, screen_h, fonts)
    }

    pub(crate) fn render_edit_host(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        self.render_host_address_form(painter, text_cache, fonts, screen_w, screen_h, "Edit address")
    }
}
