//! About screen logic: scroll/paging state. Rendering lives in `app::view::about`.
use crate::app::App;
use crate::core::screen::Screen;
use crate::ui::{self, MenuEvent};

impl App {
    /// Lazy-initialize about lines on first open.
    pub(crate) fn open_about(&mut self) {
        if self.about_lines.is_empty() {
            self.about_lines = ui::about_lines();
        }
        // `scroll` is shared with Settings' row list — stash it (see `settings_scroll`).
        self.settings_scroll = self.scroll;
        self.scroll = ui::ScrollWindow::new();
        self.content_window = ui::ContentWindow::new();
        self.screen = Screen::About;
    }

    /// Navigate: Up/Down scroll by line, Left/Right by page.
    pub(crate) fn handle_about_event(&mut self, ev: MenuEvent, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) {
        let (total, visible) = self.about_scroll_geometry(screen_w, screen_h, fonts);
        // Page step with anchor: show last few lines of previous page
        let page_step = visible.saturating_sub(2).max(1);
        match ev {
            MenuEvent::Up => {
                self.scroll.scroll_by(-1, total, visible);
            }
            MenuEvent::Down => {
                self.scroll.scroll_by(1, total, visible);
            }
            MenuEvent::Left => {
                self.scroll.page(page_step, false, total, visible);
            }
            MenuEvent::Right => {
                self.scroll.page(page_step, true, total, visible);
            }
            // Return to Settings (not Home) to preserve settings context
            MenuEvent::Back | MenuEvent::Confirm => {
                self.screen = Screen::Settings;
                self.scroll = self.settings_scroll;
            }
            MenuEvent::Secondary => {}
        }
    }

    /// Scroll by pixels (Magic Remote wheel).
    pub(crate) fn scroll_about_by(&mut self, dy_px: i32, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> bool {
        let (total, visible) = self.about_scroll_geometry(screen_w, screen_h, fonts);
        let step = ui::about_line_stride(fonts.raster, fonts.value).max(1);
        let lines = dy_px / step;
        if lines == 0 {
            return false;
        }
        self.scroll.scroll_by(i64::from(lines), total, visible)
    }

    /// Total and visible line counts.
    pub(crate) fn about_scroll_geometry(&mut self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> (usize, usize) {
        let card = ui::about_card_rect(screen_w, screen_h);
        let body = ui::about_body_rect(card, fonts);
        self.ensure_about_wrapped(fonts, body.width());
        let total = self.about_wrapped.as_ref().map_or(0, |(_, v)| v.len());
        let visible = ui::about_visible_lines(body, fonts.raster, fonts.value);
        (total, visible)
    }

    /// Defer text wrapping until width is known.
    pub(crate) fn ensure_about_wrapped(&mut self, fonts: &ui::Fonts, width: u32) {
        let stale = !matches!(&self.about_wrapped, Some((w, _)) if *w == width);
        if stale {
            self.about_wrapped = Some((
                width,
                ui::wrap_document(fonts.raster, fonts.value, &self.about_lines, width),
            ));
        }
    }
}
