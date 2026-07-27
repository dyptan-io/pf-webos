//! The per-host network speed test.
//!
//! Split out of the former single-file `app.rs`; see `super`'s module docs.
//!
//! Shape follows the pairing ceremony exactly (see `app::pairing`): the measurement
//! blocks for seconds, so it runs on a worker thread and reports back over a channel
//! drained each UI tick. Backing out drops the receiver, which orphans the worker — its
//! next send fails and it exits, tearing its own connection down.
//!
//! Measured throughput is end-to-end deliverable goodput (after AEAD decrypt),
//! not pure link speed. Bounds useful for bitrate picking on this TV.
use super::*;
use sdl2::rect::Rect;
use std::time::Instant;

use punktfunk_core::client::ProbeOutcome;

use crate::ui::{self, MenuEvent, Painter};

/// Fraction of the measured goodput to recommend as a bitrate, leaving headroom for
/// FEC overhead and real-world loss. Matches every other punktfunk client.
const RECOMMEND_NUMERATOR: u32 = 7;
const RECOMMEND_DENOMINATOR: u32 = 10;

/// Below this the measurement carried too little signal to recommend anything.
const MIN_USEFUL_KBPS: u32 = 2_000;

/// Where a running/finished speed test has got to.
pub(crate) enum SpeedTestState {
    Connecting,
    /// The burst is running; `partial` is the latest poll, if any has landed yet.
    Measuring {
        partial: Option<ProbeOutcome>,
    },
    /// `confirmed` is false if host's end-of-burst report didn't arrive.
    Done {
        outcome: ProbeOutcome,
        confirmed: bool,
    },
    Failed(String),
}

/// What the worker sends back.
pub(crate) enum SpeedTestMsg {
    Progress(ProbeOutcome),
    Done {
        outcome: Box<ProbeOutcome>,
        confirmed: bool,
    },
    Failed(String),
}

impl App {
    /// Opens `Screen::SpeedTest` for sidebar entry `idx` and starts the probe.
    pub(crate) fn open_speed_test(&mut self, idx: usize) {
        let Some(entry) = self.entries.get(idx) else { return };
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        // Saved host: pinned fingerprint. Unpaired: TOFU (no persistence on test).
        let pin = self
            .known_hosts
            .iter()
            .find(|h| h.host == host && h.port == port)
            .and_then(|h| h.fingerprint);

        self.speed_test_name = name;
        self.speed_test = Some(SpeedTestState::Connecting);
        self.speed_test_focused = 0;
        self.screen = Screen::SpeedTest;
        tracing::info!("speed test: connecting to {host}:{port}");

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.speed_test_rx = Some(rx);
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = crate::session::run_speed_probe(
                &host,
                port,
                identity,
                pin,
                std::time::Duration::from_secs(20),
                |partial| {
                    let _ = progress_tx.send(SpeedTestMsg::Progress(partial));
                },
            );
            let _ = match result {
                Ok(r) => tx.send(SpeedTestMsg::Done {
                    outcome: Box::new(r.outcome),
                    confirmed: r.confirmed,
                }),
                Err(e) => tx.send(SpeedTestMsg::Failed(crate::errors::friendly(&e))),
            };
        });
    }

    /// Drains the worker's updates, if any — called each tick alongside the other
    /// `drain_*`s. Returns whether anything changed.
    pub(crate) fn drain_speed_test(&mut self) -> bool {
        let Some(rx) = &self.speed_test_rx else { return false };
        let mut changed = false;
        // WHY: keep only latest; burst between ticks costs one redraw, not per-message.
        while let Ok(msg) = rx.try_recv() {
            changed = true;
            match msg {
                SpeedTestMsg::Progress(p) => {
                    self.speed_test = Some(SpeedTestState::Measuring { partial: Some(p) });
                }
                SpeedTestMsg::Done { outcome, confirmed } => {
                    tracing::info!(
                        "speed test: {} kbps, {:.1}% loss, {} bytes in {} ms (confirmed={confirmed})",
                        outcome.throughput_kbps,
                        outcome.loss_pct,
                        outcome.recv_bytes,
                        outcome.elapsed_ms
                    );
                    self.speed_test = Some(SpeedTestState::Done {
                        outcome: *outcome,
                        confirmed,
                    });
                    self.speed_test_focused = 0;
                    self.speed_test_rx = None;
                    break;
                }
                SpeedTestMsg::Failed(e) => {
                    tracing::warn!("speed test failed: {e}");
                    self.speed_test = Some(SpeedTestState::Failed(e));
                    self.speed_test_rx = None;
                    break;
                }
            }
        }
        changed
    }

    /// The bitrate to recommend from a finished measurement, in kbps — `None` when too
    /// little got through to say anything useful. Clamped to the settings slider's own
    /// range, since that's the only thing "Use this" can actually write.
    pub(crate) fn recommended_kbps(outcome: &ProbeOutcome) -> Option<u32> {
        if outcome.throughput_kbps < MIN_USEFUL_KBPS {
            return None;
        }
        let raw = outcome.throughput_kbps / RECOMMEND_DENOMINATOR * RECOMMEND_NUMERATOR;
        // Whole Mbps, clamped to slider bounds (BITRATE_STEP_KBPS steps).
        let whole_mbps = (raw / 1000).max(1) * 1000;
        Some(whole_mbps.clamp(ui::BITRATE_MIN_KBPS, ui::BITRATE_MAX_KBPS))
    }

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

    pub(crate) fn handle_speed_test_event(&mut self, ev: MenuEvent) {
        let done = matches!(
            self.speed_test,
            Some(SpeedTestState::Done { .. }) | Some(SpeedTestState::Failed(_))
        );
        match ev {
            // Back cancels (drops receiver → orphans worker → tears connection).
            MenuEvent::Back => self.close_speed_test(),
            _ if !done => {}
            MenuEvent::Left | MenuEvent::Right => {
                self.speed_test_focused = 1 - self.speed_test_focused;
                self.modal_focus_anim = Some(Instant::now());
            }
            MenuEvent::Confirm => {
                if self.speed_test_focused != 0 {
                    self.close_speed_test();
                    return;
                }
                let applied = match &self.speed_test {
                    Some(SpeedTestState::Done { outcome, .. }) => Self::recommended_kbps(outcome),
                    _ => None,
                };
                match applied {
                    Some(kbps) => {
                        self.settings.bitrate_kbps = kbps;
                        self.settings_writer.save(self.settings);
                        self.close_speed_test();
                    }
                    None => self.retry_speed_test(),
                }
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Re-runs the probe against the host this screen was opened for. The host menu's
    /// index is still set (this screen is only ever reached from there), so nothing has
    /// to be stashed separately.
    pub(crate) fn retry_speed_test(&mut self) {
        let Some(idx) = self.host_menu_index else {
            self.close_speed_test();
            return;
        };
        self.open_speed_test(idx);
    }

    /// Leaves the screen, abandoning any in-flight probe.
    pub(crate) fn close_speed_test(&mut self) {
        self.speed_test = None;
        self.speed_test_rx = None;
        self.back_to_host_menu();
    }

    /// The status sentence for the current phase — also measured (without drawing) to
    /// place the card and its buttons, so it lives in one place.
    pub(crate) fn speed_test_status(&self) -> String {
        match &self.speed_test {
            None | Some(SpeedTestState::Connecting) => {
                format!("Connecting to {}…", self.speed_test_name)
            }
            Some(SpeedTestState::Measuring { partial }) => {
                // Deliberately bytes, not Mbps: `throughput_kbps` divides by the HOST's
                // reported burst duration, which stays 0 until the end-of-burst report
                // lands — so a "Mbps so far" reading here could never show anything.
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
            let header_end = ui::modal_header_end_y(fonts.label, fonts.value, probe, &status);
            if done {
                (header_end + 32 + 72 + 32) as u32
            } else {
                (header_end + 32) as u32
            }
        })
    }

    /// The button row's rect, below the status text.
    pub(crate) fn speed_test_buttons_rect(&self, card: Rect, fonts: &ui::Fonts) -> Rect {
        let after = ui::modal_header_end_y(fonts.label, fonts.value, card, &self.speed_test_status());
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
        self.draw_modal_shell(painter, text_cache, fonts.icon, card)?;
        let failed = matches!(self.speed_test, Some(SpeedTestState::Failed(_)));
        ui::draw_modal_header(
            painter,
            text_cache,
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
