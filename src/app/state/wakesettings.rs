//! Per-host Wake-on-LAN settings — logic. Rendering lives in `app::view::wakesettings`.
use crate::app::App;
use crate::core::screen::Screen;
use crate::services::store;
use crate::ui::MenuEvent;
use std::time::Instant;

impl App {
    /// Open Wake settings for host menu's current host.
    pub(crate) fn open_wake_settings(&mut self) {
        self.wake_settings_focused = 0;
        self.screen = Screen::WakeSettings;
    }

    /// Host being edited (always from host menu).
    pub(crate) fn wake_settings_host(&self) -> Option<&store::KnownHost> {
        let entry = self.host_menu_index.and_then(|i| self.entries.get(i))?;
        let (host, port) = (entry.host(), entry.port());
        self.known_hosts.iter().find(|h| h.host == host && h.port == port)
    }

    /// Left/Right/Confirm flip toggle; Back returns to host menu.
    pub(crate) fn handle_wake_settings_event(&mut self, ev: MenuEvent) {
        let len = self.wake_settings_rows().len();
        if crate::ui::list_nav(&mut self.wake_settings_focused, len, ev) {
            self.modal_focus_anim = Some(Instant::now());
            return;
        }
        match ev {
            MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm => self.toggle_wol_auto(),
            MenuEvent::Back => self.screen = Screen::HostMenu,
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Flip auto-send flag and persist (discovered-only hosts have no record).
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
}
