use super::*;
use sdl2::rect::Rect;
use std::time::Instant;

use crate::ui::{self, FocusRow, MenuEvent, Painter};

impl App {
    /// Opens the Diagnostics screen — reached from the "Diagnostics" row at the
    /// bottom of Settings (`ui::ROW_DIAGNOSTICS`), not a hidden/remote-button menu.
    pub(crate) fn open_diagnostics(&mut self) {
        self.diagnostics_focused = 0;
        self.screen = Screen::Diagnostics;
    }

    pub(crate) fn diagnostics_rows(&self) -> Vec<FocusRow> {
        ui::diagnostics_rows(self.settings.log_level_override)
    }

    pub(crate) fn diagnostics_subtitle(&self) -> String {
        "Debug aid for on-device investigation. Applies immediately.".to_string()
    }

    pub(crate) fn diagnostics_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, subtitle: &str) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, 1)
    }

    /// Left/Right cycles log level directly; Confirm opens the same dropdown
    /// picker every other `Settings` dropdown row uses (`ui::ROW_RESOLUTION` etc.
    /// via `App::handle_settings_event`) — row is always `0` here (Diagnostics'
    /// only row), disambiguated from `Settings`' row 0 by `self.screen` (see
    /// `dropdown_overlay_tile`'s docs). Back (row list) saves and returns to
    /// Settings — same as `About`'s Back, since this is reached from there.
    pub(crate) fn handle_diagnostics_event(&mut self, ev: MenuEvent) {
        if let Some(dd) = self.dropdown.as_mut() {
            let len = ui::LOG_LEVEL_OPTIONS.len();
            match ev {
                MenuEvent::Up => dd.focused = if dd.focused == 0 { len - 1 } else { dd.focused - 1 },
                MenuEvent::Down => dd.focused = (dd.focused + 1) % len,
                MenuEvent::Confirm => {
                    let choice = dd.focused;
                    self.dropdown_fade.close((0, choice));
                    self.dropdown = None;
                    self.set_log_level(ui::LOG_LEVEL_OPTIONS[choice]);
                }
                MenuEvent::Back => {
                    self.dropdown_fade.close((0, dd.focused));
                    self.dropdown = None;
                }
                MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => {}
            }
            return;
        }
        let len = self.diagnostics_rows().len();
        if ui::list_nav(&mut self.diagnostics_focused, len, ev) {
            self.modal_focus_anim = Some(Instant::now());
            return;
        }
        match ev {
            MenuEvent::Left | MenuEvent::Right => self.cycle_log_level(),
            MenuEvent::Confirm => {
                self.dropdown = Some(DropdownState {
                    row: 0,
                    focused: ui::log_level_dropdown_current_index(self.settings.log_level_override),
                });
                self.dropdown_fade.reopen();
            }
            MenuEvent::Back => {
                self.settings_writer.save(self.settings);
                self.screen = Screen::Settings;
                self.scroll = self.settings_scroll;
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    fn set_log_level(&mut self, level: store::LogLevelOverride) {
        self.settings.log_level_override = level;
        crate::logger::set_level_override(level);
    }

    fn cycle_log_level(&mut self) {
        let idx = ui::log_level_dropdown_current_index(self.settings.log_level_override);
        let next = ui::cycle_index(idx, ui::LOG_LEVEL_OPTIONS.len(), true);
        self.set_log_level(ui::LOG_LEVEL_OPTIONS[next]);
    }

    pub(crate) fn render_diagnostics(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let subtitle = self.diagnostics_subtitle();
        let card = Self::diagnostics_card_rect(screen_w, screen_h, fonts, &subtitle);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        ui::render_list_modal(
            painter,
            text_cache,
            fonts,
            card,
            "Diagnostics",
            &subtitle,
            &self.diagnostics_rows(),
        )
    }
}
