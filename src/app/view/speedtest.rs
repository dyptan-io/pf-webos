//! The per-host network speed test — rendering. Logic lives in `app::state::speedtest`.
use crate::app::state::speedtest::SpeedTestState;
use crate::app::App;
use crate::ui::render::Rect;
use crate::ui::{self, Painter};
use anyhow::Result;

impl App {
    /// Primary button label (built from measurement, not constant).
    /// No recommendation → "Retry" (not Close) to give user action on low throughput.
    pub(crate) fn speed_test_apply_label(recommended: Option<u32>) -> String {
        recommended.map_or_else(|| "Retry".to_string(), |kbps| format!("Use {} Mbps", kbps / 1000))
    }

    /// Finished test buttons (apply recommendation or close). Built per-render.
    pub(crate) fn speed_test_buttons(apply_label: &str) -> [ui::ConfirmButton<'_>; 2] {
        [
            ui::ConfirmButton {
                icon: Some(ui::ICON_SIGNAL),
                label: apply_label,
                color: ui::ACCENT_BRIGHT,
            },
            ui::ConfirmButton {
                icon: None,
                label: "Close",
                color: ui::WHITE,
            },
        ]
    }

    /// The status sentence for the current phase — also measured (without drawing) to
    /// place the card and its buttons, so it lives in one place.
    pub(crate) fn speed_test_status(&self) -> String {
        match &self.speed_test {
            None | Some(SpeedTestState::Connecting) => {
                format!("Connecting to {}…", self.speed_test_name)
            }
            Some(SpeedTestState::Measuring { partial }) => {
                // Deliberately bytes, not Mbps: `throughput_kbps`'s denominator (since core
                // 0.24, the client-measured receive interval, falling back to the host's
                // burst duration) is frozen only when the end-of-burst report lands — so a
                // "Mbps so far" reading here could never show anything.
                // `recv_bytes` is live throughout.
                let so_far = partial
                    .filter(|p| p.recv_bytes > 0)
                    .map_or_else(String::new, |p| format!(" — {} MB in", p.recv_bytes / (1024 * 1024)));
                format!("Measuring{so_far} over the real data plane…")
            }
            Some(SpeedTestState::Done { outcome, confirmed }) => {
                // The burst deliberately asks for more than the link can carry — that
                // overshoot is *how* the ceiling is found — so a high loss figure here is
                // expected and says nothing bad about the network on its own. Labelled
                // accordingly, since a bare "80% loss" reads as a fault.
                let detail = if *confirmed {
                    format!(
                        "({:.0}% of the deliberately over-capacity test burst didn't fit — \
                         that's how the ceiling is found)",
                        outcome.loss_pct
                    )
                } else {
                    "(the host's own report didn't make it back, so this is measured from what \
                     arrived here — treat it as a floor)"
                        .to_string()
                };
                let base = format!(
                    "{} Mbps delivered · {} MB in {} ms\n{detail}",
                    outcome.throughput_kbps / 1000,
                    outcome.recv_bytes / (1024 * 1024),
                    outcome.elapsed_ms,
                );
                match Self::recommended_kbps(outcome) {
                    Some(kbps) => format!(
                        "{base}\n\nRecommended bitrate {} Mbps (~70% of measured, leaving headroom \
                         for FEC and loss). This measures what this TV can actually receive and \
                         decrypt, not raw link speed.",
                        kbps / 1000
                    ),
                    None => format!(
                        "{base}\n\nToo little got through to recommend a bitrate. If the host \
                         reported bytes sent but none arrived, the path is dropping them; if it \
                         sent none at all, it may not support the probe. The app log has both \
                         figures."
                    ),
                }
            }
            Some(SpeedTestState::Failed(e)) => format!("Couldn't measure: {e}"),
        }
    }

    pub(crate) fn speed_test_card_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::Fonts) -> Rect {
        let status = self.speed_test_status();
        let done = matches!(
            self.speed_test,
            Some(SpeedTestState::Done { .. }) | Some(SpeedTestState::Failed(_))
        );
        Self::simple_modal_card(screen_w, screen_h, |probe| {
            let header_end = ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, probe, &status);
            if done {
                (header_end + 32 + 72 + 32) as u32
            } else {
                (header_end + 32) as u32
            }
        })
    }

    /// The button row's rect, below the status text.
    pub(crate) fn speed_test_buttons_rect(&self, card: Rect, fonts: &ui::Fonts) -> Rect {
        let after = ui::modal_header_end_y(fonts.raster, fonts.label, fonts.value, card, &self.speed_test_status());
        Rect::new(card.x() + 32, after + 32, card.width().saturating_sub(64), 72)
    }

    pub(crate) fn render_speed_test(
        &self,
        painter: &mut Painter,
        text_cache: &mut crate::ui::TextCache,
        fonts: &ui::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let card = self.speed_test_card_rect(screen_w, screen_h, fonts);
        self.draw_modal_shell(painter, text_cache, fonts.raster, fonts.icon, card)?;
        let failed = matches!(self.speed_test, Some(SpeedTestState::Failed(_)));
        ui::draw_modal_header(
            painter,
            text_cache,
            fonts.raster,
            fonts.label,
            fonts.value,
            card,
            "Network speed test",
            ui::WHITE,
            &self.speed_test_status(),
            if failed { ui::ERROR_RED } else { ui::MUTED },
        )?;
        if matches!(
            self.speed_test,
            Some(SpeedTestState::Done { .. }) | Some(SpeedTestState::Failed(_))
        ) {
            let recommended = match &self.speed_test {
                Some(SpeedTestState::Done { outcome, .. }) => Self::recommended_kbps(outcome),
                _ => None,
            };
            let apply_label = Self::speed_test_apply_label(recommended);
            let buttons = Self::speed_test_buttons(&apply_label);
            // `usize::MAX` = nothing focused; the focused button is a separate
            // `Tile::ModalFocusElement` (see `prepare_tiles`).
            ui::draw_confirm_buttons(
                painter,
                text_cache,
                fonts,
                self.speed_test_buttons_rect(card, fonts),
                &buttons,
                usize::MAX,
            )?;
        }
        Ok(())
    }
}
