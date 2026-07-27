//! The per-host actions menu — the extension point for anything that acts on one host.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
//!
//! Adding an action here is deliberately two edits and nothing else: a row in
//! [`App::host_menu_actions`] and an arm in [`App::confirm_host_menu_row`]. Everything
//! else — card geometry, the unfocused shell, the focused-row tile, the focus pop — is
//! `ui::ListModal`'s, shared with any future list screen.
use super::*;
use sdl2::rect::Rect;
use std::time::Instant;

use crate::ui::{self, FocusRow, HostEntry, MenuEvent, Painter};

/// Host action (enum instead of bare index so conditional rows don't silently shift indices).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostAction {
    Connect,
    Pair,
    SpeedTest,
    Wake,
    Edit,
    Forget,
}

impl App {
    /// Opens host menu for sidebar row `idx` (⋯ button, pointer, or Right key).
    pub(crate) fn open_host_menu(&mut self, idx: usize) {
        self.host_menu_index = Some(idx);
        self.menu_focused = 0;
        self.host_menu_dots = false;
        self.screen = Screen::HostMenu;
    }

    /// Whether focused row's ⋯ button exists (only "Wake host" has one).
    pub(crate) fn host_menu_row_has_dots(&self) -> bool {
        self.host_menu_actions()
            .get(self.menu_focused)
            .is_some_and(|(a, _)| *a == HostAction::Wake)
    }

    /// Menu rows and actions; conditional on host state (saved/discovered, has MAC).
    pub(crate) fn host_menu_actions(&self) -> Vec<(HostAction, FocusRow)> {
        let Some(entry) = self.host_menu_index.and_then(|i| self.entries.get(i)) else {
            return Vec::new();
        };
        let saved = matches!(entry, HostEntry::Known(_));
        let mut rows = vec![
            (
                HostAction::Connect,
                if entry.is_paired() {
                    FocusRow::action(ui::ICON_TV, "Connect")
                } else {
                    // The hint goes in the value column like every other Action row's,
                    // rather than being parenthesised into the label.
                    FocusRow::action_with_value(ui::ICON_TV, "Connect", "pairs first")
                },
            ),
            (HostAction::Pair, FocusRow::action(ui::ICON_LOCK, "Pair with PIN…")),
            (
                HostAction::SpeedTest,
                FocusRow::action(ui::ICON_SIGNAL, "Test network speed…"),
            ),
        ];
        if !entry.mac().is_empty() {
            // The one row with a ⋯: Confirm wakes now, the button holds the per-host
            // wake settings (`Screen::WakeSettings`). Same affordance and the same
            // Right-to-reach-it gesture as a sidebar host row's. Always built
            // *un*focused — whether the button is lit is `host_menu_dots`, applied by
            // the focused-row tile alone (see `App::modal_focus_tile`), so the shell
            // underneath can't bake in a highlight that outlives it.
            rows.push((
                HostAction::Wake,
                FocusRow::action(ui::ICON_POWER, "Wake host").with_menu(false),
            ));
        }
        if saved {
            rows.push((HostAction::Edit, FocusRow::action(ui::ICON_EDIT, "Edit address…")));
            rows.push((
                HostAction::Forget,
                FocusRow::action(ui::ICON_DELETE, "Forget host").danger(),
            ));
        }
        rows
    }

    pub(crate) fn host_menu_rows(&self) -> Vec<FocusRow> {
        self.host_menu_actions().into_iter().map(|(_, r)| r).collect()
    }

    /// The host's name — the menu's title.
    pub(crate) fn host_menu_title(&self) -> String {
        self.host_menu_index
            .and_then(|i| self.entries.get(i))
            .map_or_else(String::new, |e| e.name().to_string())
    }

    /// `address:port`, plus the pairing state — the menu's subtitle.
    pub(crate) fn host_menu_subtitle(&self) -> String {
        self.host_menu_index
            .and_then(|i| self.entries.get(i))
            .map_or_else(String::new, |e| {
                format!(
                    "{}:{} · {}",
                    e.host(),
                    e.port(),
                    if e.is_paired() { "paired" } else { "not paired" }
                )
            })
    }

    pub(crate) fn host_menu_card_rect(
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::Fonts,
        subtitle: &str,
        rows: usize,
    ) -> Rect {
        ui::list_modal_card_rect(screen_w, screen_h, fonts, subtitle, rows)
    }

    /// Handles host menu events; may return `ConnectTarget` (currently never does).
    pub(crate) fn handle_host_menu_event(&mut self, ev: MenuEvent) {
        let len = self.host_menu_actions().len();
        if ui::list_nav(&mut self.menu_focused, len, ev) {
            // Vertical movement always lands on the row body — a ⋯ belongs to the row
            // it's on, so leaving that row leaves the button too.
            self.host_menu_dots = false;
            self.modal_focus_anim = Some(Instant::now());
            return;
        }
        match ev {
            // Right/Left move onto and off the focused row's ⋯, mirroring the sidebar's
            // `HomeFocus::SidebarMenu`; on a row without one they do nothing.
            MenuEvent::Right if !self.host_menu_dots && self.host_menu_row_has_dots() => {
                self.host_menu_dots = true;
                self.modal_focus_anim = Some(Instant::now());
            }
            MenuEvent::Left if self.host_menu_dots => {
                self.host_menu_dots = false;
                self.modal_focus_anim = Some(Instant::now());
            }
            MenuEvent::Confirm if self.host_menu_dots => self.open_wake_settings(),
            MenuEvent::Confirm => self.confirm_host_menu_row(),
            MenuEvent::Back => {
                self.host_menu_index = None;
                self.screen = Screen::Home;
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => {}
        }
    }

    /// Runs focused row's action; every arm navigates away or closes menu.
    pub(crate) fn confirm_host_menu_row(&mut self) {
        let actions = self.host_menu_actions();
        let Some((action, _)) = actions.get(self.menu_focused) else {
            return;
        };
        let Some(idx) = self.host_menu_index else { return };
        match action {
            HostAction::Connect => {
                self.host_menu_index = None;
                self.screen = Screen::Home;
                self.confirm_sidebar_host(idx);
            }
            // Straight to the PIN ceremony, even for an already-paired host: re-pairing
            // is the documented recovery when a host's certificate has changed.
            HostAction::Pair => {
                self.host_menu_index = None;
                self.open_pairing(idx);
            }
            HostAction::SpeedTest => self.open_speed_test(idx),
            HostAction::Wake => {
                let Some(entry) = self.entries.get(idx) else { return };
                let (host, port) = (entry.host().to_string(), entry.port());
                let mac = entry.mac().to_vec();
                let name = entry.name().to_string();
                self.host_menu_index = None;
                self.screen = Screen::Home;
                self.start_wake(host, port, mac, format!("Waking {name}…"));
            }
            HostAction::Edit => self.open_edit_host(idx),
            HostAction::Forget => self.open_forget_host(idx),
        }
    }

    pub(crate) fn render_host_menu(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let rows = self.host_menu_rows();
        let subtitle = self.host_menu_subtitle();
        let card = Self::host_menu_card_rect(screen_w, screen_h, fonts, &subtitle, rows.len());
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        ui::render_list_modal(
            painter,
            text_cache,
            fonts,
            card,
            &self.host_menu_title(),
            &subtitle,
            &rows,
        )
    }
}
