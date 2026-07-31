//! The "you can only pin N games" alert — logic. Rendering lives in `app::view::pinlimit`.
use crate::app::App;
use crate::core::screen::Screen;
use crate::ui::MenuEvent;

impl App {
    /// Shown when hold-to-pin would exceed `MAX_PINNED_GAMES` (5 items).
    pub(crate) const PIN_LIMIT_MESSAGE: &'static str =
        "You can only pin up to 5 items. Unpin something before pinning this one.";

    /// Enter `PinLimit` alert when pinning exceeds `MAX_PINNED_GAMES`.
    pub(crate) fn open_pin_limit(&mut self) {
        self.screen = Screen::PinLimit;
    }

    /// Handle `PinLimit`: OK and Back both dismiss the alert.
    pub fn handle_pin_limit_event(&mut self, ev: MenuEvent) {
        if matches!(ev, MenuEvent::Confirm | MenuEvent::Back) {
            self.screen = Screen::Home;
        }
    }
}
