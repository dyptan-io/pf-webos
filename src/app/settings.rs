//! The settings modal: its row list, dropdown overlay, and persistence.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
use super::*;
use crate::ui::{self, MenuEvent, Painter};
use anyhow::Result;
use sdl2::rect::Rect;
use std::time::Instant;

impl App {
    /// Handles one menu event on the settings modal. `screen_h` is only used by
    /// `Up`/`Down` to keep `settings_scroll` following `settings_focused`.
    pub fn handle_settings_event(&mut self, ev: MenuEvent, screen_h: u32) {
        // An open Resolution/Frame rate dropdown intercepts all input until it's
        // closed (by picking an option or backing out) — it's a modal overlay on
        // top of the settings row list.
        if let Some(dd) = self.dropdown.as_mut() {
            let row = dd.row;
            let len = ui::dropdown_options(&self.settings, row).len().max(1);
            match ev {
                MenuEvent::Up => dd.focused = if dd.focused == 0 { len - 1 } else { dd.focused - 1 },
                MenuEvent::Down => dd.focused = (dd.focused + 1) % len,
                MenuEvent::Confirm => {
                    let choice = dd.focused;
                    // Not persisted here — `MenuEvent::Back` below (leaving the
                    // whole Settings screen) saves once for every change made
                    // during this visit, not per-row.
                    ui::apply_dropdown_choice(&mut self.settings, row, choice);
                    self.dropdown = None;
                }
                MenuEvent::Back => self.dropdown = None,
                MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => {}
            }
            return;
        }
        let total = ui::SETTINGS_ROW_COUNT;
        match ev {
            // No wraparound here (unlike most other row lists) — wrapping a scrolled
            // list would silently jump the scroll position across the whole card.
            MenuEvent::Up => {
                if self.settings_focused > 0 {
                    self.settings_focused -= 1;
                    self.modal_focus_anim = Some(Instant::now());
                    self.scroll_settings_into_view(screen_h);
                }
            }
            MenuEvent::Down => {
                if self.settings_focused + 1 < total {
                    self.settings_focused += 1;
                    self.modal_focus_anim = Some(Instant::now());
                    self.scroll_settings_into_view(screen_h);
                }
            }
            MenuEvent::Left => self.apply_setting_adjust(self.settings_focused, false),
            MenuEvent::Right => self.apply_setting_adjust(self.settings_focused, true),
            MenuEvent::Confirm => match self.settings_focused {
                // Not a setting — a link out to the About screen (see `ui::ROW_ABOUT`).
                // Settings are saved on the way out so the visit's changes aren't lost
                // behind the navigation.
                ui::ROW_ABOUT => {
                    self.settings_writer.save(self.settings);
                    self.open_about();
                }
                ui::ROW_RESOLUTION | ui::ROW_FRAMERATE | ui::ROW_VIDEO_BACKEND | ui::ROW_CODEC | ui::ROW_AUDIO => {
                    let focused = ui::dropdown_current_index(&self.settings, self.settings_focused);
                    self.dropdown = Some(DropdownState {
                        row: self.settings_focused,
                        focused,
                    });
                }
                row => self.apply_setting_adjust(row, true),
            },
            // Leaving Settings (Back key or the modal's close-X, both funnel
            // through `App::back`) — save once for whatever changed during
            // this visit instead of once per row/keystroke. `settings_writer`
            // still queues the write on a background thread either way (see
            // its docs), but there's no reason to touch disk at all more than
            // once per Settings visit.
            MenuEvent::Back => {
                self.settings_writer.save(self.settings);
                self.screen = Screen::Home;
            }
            MenuEvent::Secondary => {}
        }
    }

    /// Adjusts settings row `row` in memory (see `ui::adjust_setting`) — the
    /// one place `Left`/`Right`/`Confirm` all funnel through. Not persisted
    /// here; `handle_settings_event`'s `Back` arm saves once when the whole
    /// Settings screen closes. For a `Toggle` row this also starts
    /// `switch_anim`, capturing the value it's about to flip *from* so the
    /// switch's render can slide the knob from there instead of snapping to
    /// the new state.
    pub(crate) fn apply_setting_adjust(&mut self, row: usize, forward: bool) {
        let toggled_from = match row {
            ui::ROW_HDR => Some(self.settings.hdr_enabled),
            ui::ROW_STATS_OVERLAY => Some(self.settings.stats_overlay),
            _ => None,
        };
        if ui::adjust_setting(&mut self.settings, row, forward) {
            if let Some(from) = toggled_from {
                self.switch_anim = Some((Instant::now(), from));
            }
        }
    }
    /// How many settings rows fit on screen at once, so the card scrolls instead of
    /// overflowing a 1080p-class panel. Clamped to `[1, SETTINGS_ROW_COUNT]`.
    pub(crate) fn settings_visible_rows(screen_h: u32) -> usize {
        let stride = ui::SETTINGS_ROW_H + ui::SETTINGS_ROW_GAP as u32;
        // 200 header/footer padding (mirrors `settings_layout`) + 160 edge margin.
        let available = screen_h.saturating_sub(200 + 160);
        ((available / stride) as usize).clamp(1, ui::SETTINGS_ROW_COUNT)
    }

    /// `settings_scroll` is only re-clamped on `Up`/`Down`, so it can be stale after a
    /// resize; use this instead of the raw field wherever it's subtracted from a row
    /// index, to avoid underflow.
    pub(crate) fn clamped_settings_scroll(&self, screen_h: u32) -> usize {
        let visible = Self::settings_visible_rows(screen_h);
        self.settings_scroll.min(ui::SETTINGS_ROW_COUNT.saturating_sub(visible))
    }

    /// Moves `settings_scroll` just enough to keep `settings_focused` in view, and marks
    /// the scrollbar to show briefly if the position actually moved.
    pub(crate) fn scroll_settings_into_view(&mut self, screen_h: u32) {
        let visible = Self::settings_visible_rows(screen_h);
        let before = self.settings_scroll;
        if self.settings_focused < self.settings_scroll {
            self.settings_scroll = self.settings_focused;
        } else if self.settings_focused >= self.settings_scroll + visible {
            self.settings_scroll = self.settings_focused + 1 - visible;
        }
        if self.settings_scroll != before {
            self.settings_scroll_shown_at = Some(Instant::now());
        }
    }

    /// The settings modal's card/content rects — shared by `render` and mouse
    /// hit-testing so they can never disagree. `content`'s height spans only the
    /// visible row window (`settings_visible_rows`), not the full row list.
    pub(crate) fn settings_layout(screen_w: u32, screen_h: u32) -> (Rect, Rect) {
        let visible = Self::settings_visible_rows(screen_h);
        let content_h = visible as u32 * (ui::SETTINGS_ROW_H + ui::SETTINGS_ROW_GAP as u32);
        // Room for the title/divider above and the high-bitrate caution below.
        let card_h = content_h + 200;
        // Widened from 0.56 to fit the scroll indicator on the right edge.
        let card = ui::modal_card_rect(screen_w, screen_h, 0.62, card_h);
        let content = Rect::new(
            card.x() + 40,
            card.y() + 120,
            card.width().saturating_sub(80),
            content_h,
        );
        (card, content)
    }
    pub(crate) fn render_settings(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let (card, content) = Self::settings_layout(screen_w, screen_h);
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        ui::draw_text(
            painter,
            text_cache,
            fonts.label,
            "Settings",
            card.x() + 40,
            card.y() + 36,
            ui::WHITE,
        )?;
        painter.fill_rect(
            Rect::new(card.x() + 40, card.y() + 88, card.width().saturating_sub(80), 1),
            sdl2::pixels::Color::RGBA(0xff, 0xff, 0xff, 0x1e),
        );

        // The row list itself is drawn separately — see `Tile::SettingsRows` — so
        // scrolling never re-rasterizes this shell; only a value/dropdown change does.

        if self.settings.bitrate_kbps > ui::BITRATE_WARN_KBPS {
            ui::draw_text(
                painter,
                text_cache,
                fonts.value,
                "May be unstable on Wi-Fi — try Ethernet if streaming drops.",
                content.x(),
                content.y() + content.height() as i32 + 16,
                ui::WARNING,
            )?;
        }

        if let Some(dd) = &self.dropdown {
            let options = ui::dropdown_options(&self.settings, dd.row);
            // Scroll is always frozen while a dropdown is open (`handle_settings_event`
            // routes Up/Down to the dropdown itself, not `settings_scroll`), so this
            // stays correct for the dropdown's whole lifetime.
            let overlay_rect = Self::dropdown_overlay_rect(content, dd.row - self.clamped_settings_scroll(screen_h));
            // `usize::MAX` = no option focused; the focused one is a separate
            // `Tile::DropdownFocusOption` (see `prepare_tiles`).
            ui::draw_dropdown_overlay(painter, text_cache, fonts.value, &options, usize::MAX, overlay_rect)?;
        }
        Ok(())
    }

    /// Where a dropdown opened from settings row `row` anchors its option
    /// overlay — one row below it. Shared by `render_settings` and `draw_list`,
    /// which both need it (as a whole, or per-option via
    /// `ui::dropdown_option_rect`).
    pub(crate) fn dropdown_overlay_rect(content: Rect, row: usize) -> Rect {
        let y = ui::focus_row_rect(content, row + 1).y();
        Rect::new(content.x(), y, content.width(), 0)
    }
}
