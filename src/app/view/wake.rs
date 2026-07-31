//! The "host unreachable — wake it?" modal's rendering. Logic lives in `app::state::wake`.
use crate::app::{App, WakeState};
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

impl App {
    /// Card for the no-MAC modal: an informational "Host unreachable" message with no
    /// button row (nothing to send), so it's a plain message card, not a confirm dialog.
    pub(crate) fn wake_message_card(screen_w: u32, screen_h: u32, fonts: &ui::Fonts, status: &str) -> Rect {
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            (ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, probe, status) + 32) as u32
        })
    }

    /// Wake modal card rect. With a MAC it's the shared confirmation dialog; without one
    /// it's the button-less informational card.
    pub(crate) fn wake_card_rect(screen_w: u32, screen_h: u32, wake: &WakeState, fonts: &ui::Fonts) -> Rect {
        let status = Self::wake_status_text(wake);
        if wake.mac.is_empty() {
            Self::wake_message_card(screen_w, screen_h, fonts, &status)
        } else {
            ui::confirm_dialog_card(screen_w, screen_h, fonts, &status)
        }
    }

    pub(crate) fn render_wake(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let Some(wake) = &self.wake else { return Ok(()) };
        let status = Self::wake_status_text(wake);

        // With a MAC it's the shared confirmation dialog (card + Wake/Cancel row); without
        // one it's a button-less informational card ("unreachable", status explains why —
        // drain_discovery reconnects automatically once the host reappears on mDNS).
        let (card, buttons) = if wake.mac.is_empty() {
            (Self::wake_message_card(screen_w, screen_h, fonts, &status), None)
        } else {
            let (card, content) = ui::confirm_dialog_layout(screen_w, screen_h, fonts, &status);
            (card, Some(content))
        };

        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            Self::wake_title(wake),
            ui::WHITE,
            &status,
            ui::MUTED,
        )?;
        if let Some(content) = buttons {
            // usize::MAX = no focus; focused button is a separate ModalFocusElement.
            ui::draw_confirm_buttons(painter, text_cache, fonts, content, &Self::wake_buttons(), usize::MAX)?;
        }
        Ok(())
    }

    /// Wake/Cancel button pair for `render_wake` and focused-button tile.
    pub(crate) fn wake_buttons() -> [ui::ConfirmButton<'static>; 2] {
        [
            ui::ConfirmButton {
                icon: Some(ui::ICON_POWER),
                label: "Wake host",
                color: ui::ACCENT_BRIGHT,
            },
            ui::ConfirmButton {
                icon: None,
                label: "Cancel",
                color: ui::WHITE,
            },
        ]
    }

    /// Modal title varies: with MAC it's an action ("Wake this host?"), without it's state.
    pub(crate) fn wake_title(wake: &WakeState) -> &'static str {
        if wake.mac.is_empty() {
            "Host unreachable"
        } else if wake.sent {
            "Waking host…"
        } else {
            "Wake this host?"
        }
    }

    /// Wake modal status line; reconstructible from wake alone (used by render and layout).
    pub(crate) fn wake_status_text(wake: &WakeState) -> String {
        if wake.mac.is_empty() {
            format!(
                "{} isn't responding, and no Wake-on-LAN address is on record for it yet, so it \
                 can't be woken from here. It will reconnect automatically once it's back online.",
                wake.name
            )
        } else if wake.sent {
            format!("Wake signal sent to {}. Waiting for it to come back online…", wake.name)
        } else {
            format!("{} isn't responding. It may be powered off or asleep.", wake.name)
        }
    }
}
