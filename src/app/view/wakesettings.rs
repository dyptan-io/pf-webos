//! Per-host Wake-on-LAN settings — rendering. Logic lives in `app::state::wakesettings`.
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, FocusRow, Painter};
use anyhow::Result;

impl App {
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
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
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
