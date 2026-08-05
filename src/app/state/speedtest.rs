//! The per-host network speed test — logic.
//!
//! Shape follows the pairing ceremony exactly (see `app::state::pairing`): the
//! measurement blocks for seconds, so it runs on a worker thread and reports back over a
//! channel drained each UI tick. Backing out drops the receiver, which orphans the
//! worker — its next send fails and it exits, tearing its own connection down.
//!
//! Measured throughput is end-to-end deliverable goodput (after AEAD decrypt), not pure
//! link speed. Bounds useful for bitrate picking on this TV.
//!
//! Rendering lives in `app::view::speedtest`.
use crate::app::App;
use crate::core::screen::Screen;
use crate::ui::{self, MenuEvent};
use std::time::Instant;

use punktfunk_core::client::ProbeOutcome;

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
        // Only reachable for a paired host (see `App::host_menu_actions`), so the pin is
        // expected to be there; `None` still just falls back to TOFU rather than failing.
        let pin = self
            .known_hosts
            .iter()
            .find(|h| h.host == host && h.port == port)
            .and_then(crate::services::store::KnownHost::pin);

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
                crate::services::budget::SPEED_TEST,
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
}
