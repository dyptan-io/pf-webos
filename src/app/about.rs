//! The About & licenses screen's state: which line the document is scrolled to.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
//! Layout and drawing live in `ui::about` — including why only the visible window is
//! ever rasterized.
use super::*;
use sdl2::rect::Rect;

use crate::ui::{self, MenuEvent, Painter};

impl App {
    /// Enters `Screen::About`, building the document on first open (see
    /// `ui::about_lines`).
    pub(crate) fn open_about(&mut self) {
        if self.about_lines.is_empty() {
            self.about_lines = ui::about_lines();
        }
        self.about_scroll = 0;
        self.screen = Screen::About;
    }

    /// Up/Down scroll by a line, Left/Right by a page. There is nothing focusable on
    /// this screen, so every event is either scrolling or leaving.
    pub(crate) fn handle_about_event(&mut self, ev: MenuEvent, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) {
        let page = self.about_page_lines(screen_w, screen_h, fonts);
        // Keep a couple of lines of the previous screenful visible when paging, so the
        // reader has an anchor rather than a hard cut.
        let page_step = page.saturating_sub(2).max(1);
        let max = self.about_lines.len().saturating_sub(1);
        match ev {
            MenuEvent::Up => self.about_scroll = self.about_scroll.saturating_sub(1),
            MenuEvent::Down => self.about_scroll = (self.about_scroll + 1).min(max),
            MenuEvent::Left => self.about_scroll = self.about_scroll.saturating_sub(page_step),
            MenuEvent::Right => self.about_scroll = (self.about_scroll + page_step).min(max),
            // Back returns to Settings, where this screen was opened from — not Home,
            // which would throw away the settings context the user was in.
            MenuEvent::Back | MenuEvent::Confirm => self.screen = Screen::Settings,
            MenuEvent::Secondary => {}
        }
    }

    /// Scrolls by `dy_px` worth of lines — the Magic Remote's wheel. Returns whether
    /// the position actually moved (drives the redraw).
    pub(crate) fn scroll_about_by(&mut self, dy_px: i32, fonts: &ui::Fonts) -> bool {
        let step = (fonts.value.height() + 4).max(1);
        let lines = dy_px / step;
        if lines == 0 {
            return false;
        }
        let max = self.about_lines.len().saturating_sub(1);
        let next = (self.about_scroll as i64 + i64::from(lines)).clamp(0, max as i64) as usize;
        let changed = next != self.about_scroll;
        self.about_scroll = next;
        changed
    }

    /// How many source lines one screenful holds at the current geometry.
    pub(crate) fn about_page_lines(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> usize {
        let card = ui::about_card_rect(screen_w, screen_h);
        let body = ui::about_body_rect(card, fonts);
        ui::about_visible_lines(body, fonts.value)
    }

    pub(crate) fn about_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::about_card_rect(screen_w, screen_h)
    }

    pub(crate) fn render_about(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = ui::about_card_rect(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.label,
            fonts.value,
            card,
            "About & licenses",
            ui::WHITE,
            &format!("Version {}", ui::VERSION),
            ui::MUTED,
        )?;
        let body = ui::about_body_rect(card, fonts);
        ui::draw_about_body(painter, fonts.value, &self.about_lines, self.about_scroll, body)
    }
}
