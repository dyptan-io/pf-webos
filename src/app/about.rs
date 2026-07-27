use super::*;
use sdl2::rect::Rect;

use crate::ui::{self, MenuEvent, Painter};

impl App {
    /// Lazy-initialize about lines on first open.
    pub(crate) fn open_about(&mut self) {
        if self.about_lines.is_empty() {
            self.about_lines = ui::about_lines();
        }
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
            MenuEvent::Back | MenuEvent::Confirm => self.screen = Screen::Settings,
            MenuEvent::Secondary => {}
        }
    }

    /// Scroll by pixels (Magic Remote wheel).
    pub(crate) fn scroll_about_by(&mut self, dy_px: i32, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> bool {
        let (total, visible) = self.about_scroll_geometry(screen_w, screen_h, fonts);
        let step = ui::about_line_stride(fonts.value).max(1);
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
        let visible = ui::about_visible_lines(body, fonts.value);
        (total, visible)
    }

    /// Defer text wrapping until width is known.
    pub(crate) fn ensure_about_wrapped(&mut self, fonts: &ui::Fonts, width: u32) {
        let stale = !matches!(&self.about_wrapped, Some((w, _)) if *w == width);
        if stale {
            self.about_wrapped = Some((width, ui::wrap_document(fonts.value, &self.about_lines, width)));
        }
    }

    pub(crate) fn about_card_rect(screen_w: u32, screen_h: u32) -> Rect {
        ui::about_card_rect(screen_w, screen_h)
    }

    /// The shell only — header and card chrome. The document body is its own
    /// `Tile::ScrollContent(Screen::About)` tile, composited separately (see the
    /// module docs), so this no longer depends on scroll position at all.
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
        Ok(())
    }
}
