//! The "Forget this host?" confirmation modal's logic. Rendering lives in `app::view::forget`.
use crate::app::App;
use crate::core::screen::Screen;
use crate::ui::MenuEvent;
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
}
