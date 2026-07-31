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
    /// `Up`/`Down` to keep `self.scroll` following `settings_focused`.
    pub fn handle_settings_event(&mut self, ev: MenuEvent, screen_h: u32) {
        // An open Resolution/Frame rate dropdown intercepts all input until it's
        // closed (by picking an option or backing out) — it's a modal overlay on
        // top of the settings row list.
        if let Some(dd) = self.dropdown.as_mut() {
            // `dd.row` is the display position; setting lookups need the logical row.
            let row = dd.row;
            let logical = ui::settings_logical_row(&self.settings, row);
            let len = ui::dropdown_options(&self.settings, logical).len().max(1);
            match ev {
                MenuEvent::Up => dd.focused = if dd.focused == 0 { len - 1 } else { dd.focused - 1 },
                MenuEvent::Down => dd.focused = (dd.focused + 1) % len,
                MenuEvent::Confirm => {
                    let choice = dd.focused;
                    // Not persisted here — `MenuEvent::Back` below (leaving the
                    // whole Settings screen) saves once for every change made
                    // during this visit, not per-row.
                    ui::apply_dropdown_choice(&mut self.settings, logical, choice);
                    self.dropdown_fade.close((row, dd.focused));
                    self.dropdown = None;
                    // A codec change hides/shows the HDR row above; keep focus on the
                    // row just edited rather than letting the shift slide it away.
                    self.refocus_logical(logical);
                }
                MenuEvent::Back => {
                    self.dropdown_fade.close((row, dd.focused));
                    self.dropdown = None;
                }
                MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => {}
            }
            return;
        }
        let total = ui::settings_row_count(&self.settings);
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
            MenuEvent::Confirm => match ui::settings_logical_row(&self.settings, self.settings_focused) {
                // Not a setting — a link out to the About screen (see `ui::ROW_ABOUT`).
                // Settings are saved on the way out so the visit's changes aren't lost
                // behind the navigation.
                ui::ROW_ABOUT => {
                    self.settings_writer.save(self.settings);
                    self.open_about();
                }
                ui::ROW_EXPERIMENTAL => {
                    self.settings_writer.save(self.settings);
                    self.open_experimental();
                }
                ui::ROW_DIAGNOSTICS => {
                    self.settings_writer.save(self.settings);
                    self.open_diagnostics();
                }
                logical @ (ui::ROW_RESOLUTION
                | ui::ROW_FRAMERATE
                | ui::ROW_VIDEO_BACKEND
                | ui::ROW_CODEC
                | ui::ROW_AUDIO
                | ui::ROW_GAMEPAD
                | ui::ROW_COLOR_RANGE) => {
                    let focused = ui::dropdown_current_index(&self.settings, logical);
                    // `row` is the display position (what the overlay is drawn against);
                    // the logical row is recovered on lookup via `settings_logical_row`.
                    self.dropdown = Some(DropdownState {
                        row: self.settings_focused,
                        focused,
                    });
                    self.dropdown_fade.reopen();
                }
                _ => self.apply_setting_adjust(self.settings_focused, true),
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

    /// Adjusts row in memory; persisted on `Back` (not per-keystroke). Starts `switch_anim` for toggle slides.
    /// `display_row` is the on-screen position; resolved to a logical `ROW_*` first.
    pub(crate) fn apply_setting_adjust(&mut self, display_row: usize, forward: bool) {
        let row = ui::settings_logical_row(&self.settings, display_row);
        let toggled_from = match row {
            ui::ROW_HDR => Some(self.settings.hdr_enabled),
            _ => None,
        };
        if ui::adjust_setting(&mut self.settings, row, forward) {
            if let Some(from) = toggled_from {
                self.switch_anim = Some((Instant::now(), from));
            }
        }
        // Cycling the codec can hide/show the HDR row above; keep focus on `row`.
        self.refocus_logical(row);
    }

    /// After a mutation that may have shown or hidden rows (a codec change toggles the
    /// HDR row's visibility), re-derive the display index of `logical` so focus stays on
    /// the same setting instead of sliding to whatever now occupies its old slot.
    fn refocus_logical(&mut self, logical: usize) {
        let rows = ui::settings_visible_logical_rows(&self.settings);
        self.settings_focused = rows
            .iter()
            .position(|&r| r == logical)
            .unwrap_or_else(|| self.settings_focused.min(rows.len().saturating_sub(1)));
    }
    /// How many settings rows are *fully* visible. Capped at the live row count so a hidden
    /// row (Color range on NDL) leaves no empty slot.
    ///
    /// When the list overflows, one row's worth of budget is spent on `SETTINGS_PEEK` instead
    /// — the partially-visible sliver the bottom fade dissolves. Computed without the peek
    /// first, because a list that fits entirely has nothing below to peek at and should not
    /// give up the space.
    pub(crate) fn settings_visible_rows(&self, screen_h: u32) -> usize {
        let stride = ui::settings_row_stride();
        let total = ui::settings_row_count(&self.settings);
        let budget =
            screen_h.saturating_sub(ui::SETTINGS_CHROME_TOP + self.settings_chrome_bottom() + ui::SETTINGS_EDGE_MARGIN);
        if (budget / stride) as usize >= total {
            return total.max(1);
        }
        // Both peeks come out of the budget, not just the bottom one — see `SETTINGS_PEEK`.
        ((budget.saturating_sub(2 * ui::SETTINGS_PEEK) / stride) as usize).clamp(1, total)
    }

    /// Card space below the list: minimal, unless the high-bitrate caution line needs room.
    pub(crate) fn settings_chrome_bottom(&self) -> u32 {
        if self.settings.bitrate_kbps > ui::BITRATE_WARN_KBPS {
            ui::SETTINGS_WARN_CHROME
        } else {
            ui::SETTINGS_CHROME_BOTTOM
        }
    }

    /// Height of the scrolling viewport: the fully-visible rows plus a peek strip past each
    /// edge while the list overflows. Deliberately *not* a whole multiple of the row stride
    /// when scrolling — see [`ui::SETTINGS_PEEK`].
    pub(crate) fn settings_content_h(&self, screen_h: u32) -> u32 {
        let visible = self.settings_visible_rows(screen_h);
        let peeks = if visible < ui::settings_row_count(&self.settings) {
            2 * ui::SETTINGS_PEEK
        } else {
            0
        };
        visible as u32 * ui::settings_row_stride() + peeks
    }

    /// Scrolls `settings_focused` into view; updates scroll indicator.
    pub(crate) fn scroll_settings_into_view(&mut self, screen_h: u32) {
        let visible = self.settings_visible_rows(screen_h);
        self.scroll
            .scroll_into_view(self.settings_focused, ui::settings_row_count(&self.settings), visible);
    }

    /// Settings card and content rects (shared by render and hit-test).
    pub(crate) fn settings_layout(&self, screen_w: u32, screen_h: u32) -> (Rect, Rect) {
        let content_h = self.settings_content_h(screen_h);
        let card_h = content_h + ui::SETTINGS_CHROME_TOP + self.settings_chrome_bottom();
        // Widened from 0.56 to fit the scroll indicator on the right edge.
        let card = ui::modal_card_rect(screen_w, screen_h, 0.62, card_h);
        let content = Rect::new(
            card.x() + 40,
            card.y() + ui::SETTINGS_CHROME_TOP as i32,
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
        let (card, content) = self.settings_layout(screen_w, screen_h);
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

        // The row list itself is drawn separately — see `Tile::ScrollContent` — so
        // scrolling never re-rasterizes this shell; only a value/dropdown change does.
        // The open dropdown's panel is drawn separately too — see `Tile::DropdownOverlay`
        // — so it composites *after* `Tile::ScrollContent` instead of being covered by it.

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
        Ok(())
    }

    /// Where a dropdown opened from settings row `row` anchors its option
    /// overlay — one row below it. Shared by `render_settings` and `draw_list`,
    /// which both need it (as a whole, or per-option via
    /// `ui::dropdown_option_rect`).
    /// Positioned from a pixel scroll offset rather than a viewport-local row index, since a
    /// gliding list puts its rows at continuous offsets. `scroll_px` of 0 is the unscrolled
    /// case (Diagnostics).
    pub(crate) fn dropdown_overlay_rect_at_px(content: Rect, row: usize, scroll_px: i32) -> Rect {
        let y = ui::focus_row_rect_at_px(content, row + 1, scroll_px).y();
        Rect::new(content.x(), y, content.width(), 0)
    }

    /// `(row, focused, alpha)` for the open dropdown or its close-fade; `None` if neither.
    pub(crate) fn dropdown_draw_state(&self) -> Option<(usize, usize, f32)> {
        if let Some(dd) = &self.dropdown {
            Some((dd.row, dd.focused, self.dropdown_fade.open_alpha(DROPDOWN_FADE)))
        } else {
            self.dropdown_fade
                .closing_frame(DROPDOWN_FADE)
                .map(|(alpha, (row, focused))| (row, focused, alpha))
        }
    }
}
