use super::*;
use sdl2::rect::Rect;
use std::time::Instant;

use crate::ui::{self, FocusRow, MenuEvent, Painter};

impl App {
    /// Opens the Experimental screen (Settings → `ui::ROW_EXPERIMENTAL`). Holds unstable,
    /// off-by-default toggles (the frame pacer).
    pub(crate) fn open_experimental(&mut self) {
        self.experimental_focused = 0;
        // Stash scroll so Back can restore it; Experimental doesn't use it.
        self.settings_scroll = self.scroll;
        self.screen = Screen::Experimental;
    }

    pub(crate) fn experimental_rows(&self) -> Vec<FocusRow> {
        ui::experimental_rows(&self.settings)
    }

    pub(crate) fn experimental_subtitle(&self) -> String {
        "Unstable, off by default. Frame pacer also toggles live with the Blue button.".to_string()
    }

    pub(crate) fn experimental_card_rect(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, subtitle: &str) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, ui::EXPERIMENTAL_ROW_COUNT)
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
                self.switch_anim = Some((Instant::now(), from));
            }
            (_, MenuEvent::Back) => {
                self.settings_writer.save(self.settings);
                self.screen = Screen::Settings;
                self.scroll = self.settings_scroll;
            }
            _ => {}
        }
    }

    pub(crate) fn render_experimental(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let subtitle = self.experimental_subtitle();
        let card = Self::experimental_card_rect(screen_w, screen_h, fonts, &subtitle);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        ui::render_list_modal(
            painter,
            text_cache,
            fonts,
            card,
            "Experimental",
            &subtitle,
            &self.experimental_rows(),
        )
    }
}
