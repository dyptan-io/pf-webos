use super::*;
use sdl2::rect::Rect;
use std::time::Instant;

use crate::ui::{self, FocusRow, MenuEvent, Painter};

impl App {
    /// Opens the Diagnostics screen — reached from the "Diagnostics" row at the
    /// bottom of Settings (`ui::ROW_DIAGNOSTICS`), not a hidden/remote-button menu.
    pub(crate) fn open_diagnostics(&mut self) {
        self.diagnostics_focused = 0;
        // Stash scroll so Back can restore it; Diagnostics doesn't use it.
        self.settings_scroll = self.scroll;
        self.screen = Screen::Diagnostics;
    }

    pub(crate) fn diagnostics_rows(&self) -> Vec<FocusRow> {
        ui::diagnostics_rows(&self.settings)
    }

    pub(crate) fn diagnostics_subtitle(&self) -> String {
        "Debug aids for on-device investigation.".to_string()
    }

    pub(crate) fn diagnostics_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, subtitle: &str) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, ui::DIAGNOSTICS_ROW_COUNT)
    }

    /// Three rows (`ui::DIAG_ROW_*`): Log level (Left/Right cycles, Confirm opens the
    /// same dropdown picker every `Settings` dropdown uses — its row `0` is
    /// disambiguated from `Settings`' row 0 by `self.screen`, see
    /// `dropdown_overlay_tile`'s docs), Stats overlay, and Show logs (both
    /// Left/Right/Confirm toggle). Back saves and returns to Settings — this is
    /// reached from there.
    pub(crate) fn handle_diagnostics_event(&mut self, ev: MenuEvent) {
        if let Some(dd) = self.dropdown.as_mut() {
            let len = ui::LOG_LEVEL_OPTIONS.len();
            match ev {
                MenuEvent::Up => dd.focused = if dd.focused == 0 { len - 1 } else { dd.focused - 1 },
                MenuEvent::Down => dd.focused = (dd.focused + 1) % len,
                MenuEvent::Confirm => {
                    let choice = dd.focused;
                    self.dropdown_fade.close((ui::DIAG_ROW_LOG_LEVEL, choice));
                    self.dropdown = None;
                    self.set_log_level(ui::LOG_LEVEL_OPTIONS[choice]);
                }
                MenuEvent::Back => {
                    self.dropdown_fade.close((ui::DIAG_ROW_LOG_LEVEL, dd.focused));
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
        match (self.diagnostics_focused, ev) {
            (ui::DIAG_ROW_LOG_LEVEL, MenuEvent::Left | MenuEvent::Right) => self.cycle_log_level(),
            (ui::DIAG_ROW_LOG_LEVEL, MenuEvent::Confirm) => {
                self.dropdown = Some(DropdownState {
                    row: ui::DIAG_ROW_LOG_LEVEL,
                    focused: ui::log_level_dropdown_current_index(self.settings.log_level_override),
                });
                self.dropdown_fade.reopen();
            }
            (ui::DIAG_ROW_STATS_OVERLAY, MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm) => {
                let from = self.settings.stats_overlay;
                self.settings.stats_overlay = !from;
                self.switch_anim = Some((Instant::now(), from));
            }
            (ui::DIAG_ROW_SHOW_LOGS, MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm) => {
                let from = self.settings.show_logs;
                self.settings.show_logs = !from;
                crate::real::set_log_overlay_enabled(!from);
                self.switch_anim = Some((Instant::now(), from));
            }
            (ui::DIAG_ROW_SEND_LOGS, MenuEvent::Confirm) => {
                // Persist any pending diagnostics changes before leaving the screen —
                // the confirmation modal's buttons return to Home, not back here.
                self.settings_writer.save(self.settings);
                self.open_send_logs();
            }
            (_, MenuEvent::Back) => {
                self.settings_writer.save(self.settings);
                self.screen = Screen::Settings;
                self.scroll = self.settings_scroll;
            }
            _ => {}
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
