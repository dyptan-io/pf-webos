//! The "Forget this host?" confirmation modal's rendering. Logic lives in `app::state::forget`.
use crate::app::App;
use crate::ui::{self, HostEntry, Painter};
use anyhow::Result;

impl App {
    pub(crate) fn render_forget_host(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let Some(name) = self
            .host_menu_index
            .and_then(|i| self.entries.get(i))
            .map(HostEntry::name)
        else {
            return Ok(());
        };
        let (card, content) = ui::confirm_dialog_layout(screen_w, screen_h, fonts, &Self::forget_host_subtitle(name));
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;

        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            "Forget this host?",
            ui::WHITE,
            &Self::forget_host_subtitle(name),
            ui::MUTED,
        )?;

        ui::draw_confirm_buttons(painter, text_cache, fonts, content, &Self::forget_buttons(), usize::MAX)?;
        Ok(())
    }

    /// The Forget/Cancel button pair — shared by `render_forget_host`'s shell
    /// and the focused-button tile (`prepare_tiles`), so their `ConfirmButton`
    /// data can't drift apart.
    pub(crate) fn forget_buttons() -> [ui::ConfirmButton<'static>; 2] {
        ui::confirm_buttons(Some(ui::ICON_DELETE), "Forget", ui::ERROR_RED)
    }

    pub(crate) fn forget_host_subtitle(name: &str) -> String {
        format!("{name} will be removed from this TV. You can pair with it again later.")
    }
}
