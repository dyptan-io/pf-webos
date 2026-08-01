//! Experimental screen logic. Rendering lives in `app::view::experimental`.
use crate::app::App;
use crate::core::screen::Screen;
use crate::ui::{self, MenuEvent};
use std::time::Instant;

impl App {
    /// Opens the Experimental screen (Settings → `ui::ROW_EXPERIMENTAL`). Holds unstable,
    /// off-by-default toggles (the frame pacer).
    pub(crate) fn open_experimental(&mut self) {
        self.experimental_focused = 0;
        // Stash scroll so Back can restore it; Experimental doesn't use it.
        self.settings_scroll = self.scroll;
        self.screen = Screen::Experimental;
    }

    /// All rows are plain Left/Right/Confirm toggles. Back saves and returns to Settings.
    pub(crate) fn handle_experimental_event(&mut self, ev: MenuEvent) {
        let len = self.experimental_rows().len();
        if ui::list_nav(&mut self.experimental_focused, len, ev) {
            self.modal_focus_anim = Some(Instant::now());
            return;
        }
        match (self.experimental_focused, ev) {
            (ui::EXP_ROW_FRAME_PACER, MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm) => {
                let from = self.settings.video_pacing;
                self.settings.video_pacing = !from;
                self.switch_anim = Some((Instant::now(), from, self.experimental_focused));
            }
            (ui::EXP_ROW_GAME_MODE, MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm) => {
                let from = self.settings.game_mode;
                self.settings.game_mode = !from;
                self.switch_anim = Some((Instant::now(), from, self.experimental_focused));
            }
            (_, MenuEvent::Back) => {
                self.settings_writer.save(self.settings);
                self.screen = Screen::Settings;
                self.scroll = self.settings_scroll;
            }
            _ => {}
        }
    }
}
