//! Per-host Wake-on-LAN settings — the ⋯ on the host menu's "Wake host" row.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs. Built on
//! `ui::ListModal` exactly like `hostmenu.rs`, so this module is only the rows, what
//! Confirm does to them, and where Back goes.
//!
//! One toggle today (`KnownHost::wol_auto`). It used to be a global `Settings` field
//! flipped from inside the wake prompt itself, which made it reachable only by having
//! a host fail on you, and applied it to every host at once.
use super::*;
use sdl2::rect::Rect;
use std::time::Instant;

use crate::ui::{self, FocusRow, MenuEvent, Painter};

impl App {
    /// Opens the Wake settings for the host menu's current host. Keeps
    /// `host_menu_index` set: Back returns to the menu this was opened from, and the
    /// rows are read straight off that host.
    pub(crate) fn open_wake_settings(&mut self) {
        self.wake_settings_focused = 0;
        self.screen = Screen::WakeSettings;
    }

    /// The host this screen is editing — always the host menu's, since that's the only
    /// way in.
    pub(crate) fn wake_settings_host(&self) -> Option<&store::KnownHost> {
        let entry = self.host_menu_index.and_then(|i| self.entries.get(i))?;
        let (host, port) = (entry.host(), entry.port());
        self.known_hosts.iter().find(|h| h.host == host && h.port == port)
    }

    pub(crate) fn wake_settings_rows(&self) -> Vec<FocusRow> {
        ui::wake_settings_rows(self.wake_settings_host().is_some_and(|h| h.wol_auto))
    }

    pub(crate) fn wake_settings_title(&self) -> String {
        format!("Wake · {}", self.host_menu_title())
    }

    pub(crate) fn wake_settings_subtitle(&self) -> String {
        // Spells out both halves of the behaviour, because the alternative to "On" is
        // not "never wake" — it's "ask first", which the switch alone can't say.
        "On: an unreachable host is sent a wake signal straight away, retried every \
         minute until it answers. Off: it asks first."
            .to_string()
    }

    pub(crate) fn wake_settings_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, subtitle: &str) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, 1)
    }

    /// Handles one menu event. Left/Right/Confirm all flip the toggle, matching the
    /// Settings modal's toggle idiom; Back returns to the host menu.
    pub(crate) fn handle_wake_settings_event(&mut self, ev: MenuEvent) {
        let len = self.wake_settings_rows().len();
        if ui::list_nav(&mut self.wake_settings_focused, len, ev) {
            self.modal_focus_anim = Some(Instant::now());
            return;
        }
        match ev {
            MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm => self.toggle_wol_auto(),
            MenuEvent::Back => self.screen = Screen::HostMenu,
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Flips this host's auto-send flag and persists it. A discovered-but-never-saved
    /// host has no record to write to, so there's nothing to flip.
    fn toggle_wol_auto(&mut self) {
        let Some(entry) = self.host_menu_index.and_then(|i| self.entries.get(i)) else {
            return;
        };
        let (host, port) = (entry.host().to_string(), entry.port());
        let Some(known) = self.known_hosts.iter_mut().find(|h| h.host == host && h.port == port) else {
            return;
        };
        let from = known.wol_auto;
        known.wol_auto = !from;
        let _ = store::save_known_hosts(&self.known_hosts);
        // Captures the value it's flipping *from*, so the knob slides rather than
        // snapping — same contract as the Settings modal's switch rows.
        self.switch_anim = Some((Instant::now(), from));
    }

    pub(crate) fn render_wake_settings(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let subtitle = self.wake_settings_subtitle();
        let card = Self::wake_settings_card_rect(screen_w, screen_h, fonts, &subtitle);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        ui::render_list_modal(
            painter,
            text_cache,
            fonts,
            card,
            &self.wake_settings_title(),
            &subtitle,
            &self.wake_settings_rows(),
        )
    }
}
