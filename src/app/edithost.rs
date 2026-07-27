//! Editing a saved host's address, reusing the add-host entry widget.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
//!
//! Only the address is editable. The port is fixed (`ui::FIXED_HOST_PORT`), and the
//! name/fingerprint/MAC are all *learned* rather than typed — the fingerprint in
//! particular must survive an address change untouched, since it identifies the host's
//! certificate, not where it happens to sit on the network. Editing an address is
//! exactly the case where a paired host moved (new DHCP lease) and re-pairing would be
//! the wrong remedy.
use super::*;
use sdl2::rect::Rect;

use crate::store;
use crate::ui::{self, AddHostState, HostEntry, MenuEvent, Painter};

impl App {
    /// Enters `Screen::EditHost` for the sidebar row at `idx`, pre-filled with its
    /// current address. No-ops for a discovered-but-unsaved entry (there is nothing
    /// persisted to edit).
    pub(crate) fn open_edit_host(&mut self, idx: usize) {
        let Some(HostEntry::Known(h)) = self.entries.get(idx) else {
            return;
        };
        self.add_host = AddHostState::from_ip(&h.host);
        self.edit_host_index = Some(idx);
        self.host_menu_index = None;
        self.screen = Screen::EditHost;
    }

    /// Same key handling as the add-host modal — Left/Right stand in for
    /// backspace/"next octet", Confirm commits once four octets are present.
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

    /// Rewrites the edited host's address in place, keeping everything that identifies
    /// it (name, fingerprint, management port, MAC). No-ops on a still-partial address,
    /// same as `confirm_add_host`.
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

        // Drop the old record and upsert under the new address, carrying the identity
        // fields across — `upsert_known_host` keys on `(host, port)`, so a moved host
        // would otherwise leave its stale entry behind alongside the new one.
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

        // The selection follows the host to its new address rather than being dropped.
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
