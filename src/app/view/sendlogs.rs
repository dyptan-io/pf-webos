//! "Send logs to developer" modal rendering. Logic lives in `app::state::sendlogs`.
use crate::app::App;
use crate::ui::{self, Painter};
use anyhow::Result;

impl App {
    pub(crate) const SEND_LOGS_SUBTITLE: &'static str =
        "This uploads this session's log file to the app developer to help diagnose problems. \
         Logs can include host names, IP addresses, and game titles. Only send them if you're \
         comfortable sharing that.";

    /// The Send/Cancel button pair — shared by the shell render and the focused-button
    /// tile so their `ConfirmButton` data can't drift apart. Order matches
    /// `send_logs_focused` (0 = Send, 1 = Cancel); Send is drawn in the same red as
    /// the Forget action, since both are consequential.
    pub(crate) fn send_logs_buttons() -> [ui::ConfirmButton<'static>; 2] {
        ui::confirm_buttons(Some(ui::ICON_SEND), "Send", ui::ERROR_RED)
    }

    pub(crate) fn render_send_logs(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let (card, content) = ui::confirm_dialog_layout(screen_w, screen_h, fonts, Self::SEND_LOGS_SUBTITLE);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            "Send logs to developer?",
            ui::WHITE,
            Self::SEND_LOGS_SUBTITLE,
            ui::MUTED,
        )?;
        // `usize::MAX` = nothing focused here; the focused button is a separate
        // `Tile::ModalFocusElement` (see `prepare_tiles`).
        ui::draw_confirm_buttons(
            painter,
            text_cache,
            fonts,
            content,
            &Self::send_logs_buttons(),
            usize::MAX,
        )?;
        Ok(())
    }
}
