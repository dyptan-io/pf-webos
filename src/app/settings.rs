//! The settings modal: its row list, dropdown overlay, and persistence.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
use super::*;
use std::time::Instant;
use anyhow::Result;
use sdl2::rect::Rect;
use crate::ui::{self, MenuEvent, Painter};

impl App {
    /// Handles one menu event on the settings modal.
    pub fn handle_settings_event(&mut self, ev: MenuEvent) {
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
            MenuEvent::Up => {
                self.settings_focused = if self.settings_focused == 0 {
                    total - 1
                } else {
                    self.settings_focused - 1
                };
                self.modal_focus_anim = Some(Instant::now());
            }
            MenuEvent::Down => {
                self.settings_focused = (self.settings_focused + 1) % total;
                self.modal_focus_anim = Some(Instant::now());
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
    /// The settings modal's card/content rects — shared by `render` and mouse
    /// hit-testing so they can never disagree.
    pub(crate) fn settings_layout(screen_w: u32, screen_h: u32) -> (Rect, Rect) {
        let content_h = ui::SETTINGS_ROW_COUNT as u32 * (ui::SETTINGS_ROW_H + ui::SETTINGS_ROW_GAP as u32);
        // Room for the title/divider above and the high-bitrate caution below.
        let card_h = content_h + 200;
        let card = ui::modal_card_rect(screen_w, screen_h, 0.56, card_h);
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

        let rows = ui::settings_rows(&self.settings);
        // `usize::MAX` = no row focused: this shell draws every row unfocused,
        // the focused one is a separate `Tile::ModalFocusElement` (see
        // `prepare_tiles`), so moving focus never re-rasterizes the modal.
        let open_dropdown_row = self.dropdown.as_ref().map(|dd| dd.row);
        ui::draw_focus_rows(
            painter,
            text_cache,
            fonts,
            &rows,
            usize::MAX,
            open_dropdown_row,
            content,
        )?;

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
            let overlay_rect = Self::dropdown_overlay_rect(content, dd.row);
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
