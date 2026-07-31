//! The "Forget this host?" confirmation modal.
use super::*;
use crate::ui::{self, HostEntry, MenuEvent, Painter};
use anyhow::Result;
use sdl2::rect::Rect;
use std::time::Instant;

impl App {
    /// Open `ForgetHost` confirmation for sidebar row at long-press.
    pub fn open_forget_host(&mut self, idx: usize) {
        self.host_menu_index = Some(idx);
        self.host_menu_focused = 1;
        self.screen = Screen::ForgetHost;
    }

    /// Return to `HostMenu` or Home if host was removed.
    pub(crate) fn back_to_host_menu(&mut self) {
        if self.host_menu_index.is_some_and(|i| i < self.entries.len()) {
            self.menu_focused = 0;
            self.screen = Screen::HostMenu;
        } else {
            self.host_menu_index = None;
            self.screen = Screen::Home;
        }
    }

    /// Handle menu event. Left/Right toggle focus; Confirm/Back act on focused button.
    pub fn handle_forget_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left | MenuEvent::Right => {
                self.host_menu_focused = 1 - self.host_menu_focused;
                self.modal_focus_anim = Some(Instant::now());
            }
            MenuEvent::Confirm => {
                if self.host_menu_focused == 0 {
                    if let Some(idx) = self.host_menu_index {
                        self.forget_host(idx);
                    }
                    // Entry list changed; host_menu_index is now stale
                    self.host_menu_index = None;
                    self.screen = Screen::Home;
                } else {
                    self.back_to_host_menu();
                }
            }
            // Back returns to menu (not Home) to avoid closing the menu behind
            MenuEvent::Back => self.back_to_host_menu(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }
    /// Forget confirmation card rect (shared by render and hit-testing).
    pub(crate) fn forget_host_card_rect(screen_w: u32, screen_h: u32, name: &str, fonts: &ui::Fonts) -> Rect {
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            let header_end = ui::modal_header_end_y(fonts.label, fonts.value, probe, &Self::forget_host_subtitle(name));
            (header_end + 32 + 72 + 32) as u32
        })
    }
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
        let card = Self::forget_host_card_rect(screen_w, screen_h, name, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;

        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.label,
            fonts.value,
            card,
            "Forget this host?",
            ui::WHITE,
            &Self::forget_host_subtitle(name),
            ui::MUTED,
        )?;

        let content = Self::forget_host_content_rect(card, name, fonts);
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

    /// The Forget/Cancel button row's rect — depends on the host-name
    /// subtitle's wrapped height, computed via `ui::modal_header_end_y`
    /// without drawing so `prepare_tiles`/`draw_list` can position the
    /// focused-button tile without re-rendering the header.
    pub(crate) fn forget_host_content_rect(card: Rect, name: &str, fonts: &ui::Fonts) -> Rect {
        let after_subtitle_y =
            ui::modal_header_end_y(fonts.label, fonts.value, card, &Self::forget_host_subtitle(name));
        Rect::new(
            card.x() + 32,
            after_subtitle_y + 32,
            card.width().saturating_sub(64),
            72,
        )
    }
}
