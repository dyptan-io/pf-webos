//! The single place that talks to the video decoder.
//!
//! Everything between "an access unit arrived" and "NDL has been fed" lives here: host-PTS
//! anchoring, the refresh-rate-reconciled [`PtsPacer`], backpressure metering,
//! freeze-until-reanchor, and keyframe-request throttling. Protocol pumps keep only the
//! parts that are protocol-shaped — pulling frames, and *how* a keyframe is asked for.
//!
//! The seam that serves both protocols is [`SinkResult::NeedKeyframe`]: punktfunk's pump
//! answers it with `NativeClient::request_keyframe`, `GameStream` with
//! `DecodeResult::NeedIdr`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use punktfunk_core::quic;

use crate::platform::webos::ndl::NdlVideo;
use crate::session::pacing::{reconciled_pace_interval_ns, HostPtsAnchor, PtsPacer};
use crate::session::StreamStats;

/// Freeze duration after which we resume even without a clean re-anchor.
const HOLD_GIVE_UP: Duration = Duration::from_secs(2);
/// Feed calls slower than this suggest decoder backpressure rather than network loss.
const FEED_BACKPRESSURE_WARN: Duration = Duration::from_millis(20);
/// How often the sink refreshes NDL's render-buffer depth for the decode-latency signal —
/// three samples per 750 ms ABR report window; see [`NdlSink::decode_us`].
const BACKLOG_POLL: Duration = Duration::from_millis(250);

/// What the pump knows about a frame that the sink can't work out for itself.
pub struct FrameFlags {
    /// This frame can restart decoding on its own (IDR, or an LTR recovery anchor).
    pub reanchor: bool,
    /// Loss was detected at or before this frame — a sequence gap, or a frame the
    /// transport dropped.
    pub loss: bool,
    /// Host frame index, for logs only.
    pub index: u64,
}

/// Outcome of one [`NdlSink::submit`].
pub enum SinkResult {
    /// Fed to the decoder. `decode_us` is the latency figure for the host's ABR
    /// controller, present only when the sink was built with `report_decode_latency`.
    Presented { decode_us: Option<u32> },
    /// Skipped — still frozen, waiting for a re-anchor.
    Held,
    /// Skipped or failed, and the throttle allows asking the host for a keyframe now.
    /// The caller sends it however its protocol does.
    NeedKeyframe,
}

/// Video-decode backend (NDL `DirectMedia` — the only one). Arc'd because the audio-offload
/// path shares the handle with `ndl_audio_pump`; NDL unloads process-globally, so unload
/// waits for both threads (`Arc::drop`).
pub struct VideoPlayer(Arc<NdlVideo>);

impl VideoPlayer {
    pub fn new(ndl: Arc<NdlVideo>) -> Self {
        Self(ndl)
    }

    /// Feed access unit; return (result, `feed_duration`) for ABR latency reporting.
    /// `pts_ns` must already be in NDL's PTS clock domain (see [`Self::pace_base_ns`]).
    fn play(&self, au: &[u8], pts_ns: u64) -> (anyhow::Result<()>, Duration) {
        let t = Instant::now();
        let result = self.0.play(au, pts_ns);
        (result, t.elapsed())
    }

    /// Unpaced PTS reference for `frame` in NDL's clock domain, smoothed by [`PtsPacer`]
    /// before it reaches [`Self::play`]. NDL has no PTS clock of its own (see
    /// `NdlVideo::elapsed_ns`); with `anchor` present the host PTS is mapped onto NDL's
    /// player clock ([`HostPtsAnchor`]) so the reference tracks host frame cadence instead
    /// of feed-time wall-clock, keeping delivery jitter out of the pacer's drift-clamp
    /// anchor. Without `anchor` (pacing off) NDL falls back to raw player time.
    fn pace_base_ns(&self, frame_pts_ns: u64, anchor: Option<&mut HostPtsAnchor>) -> u64 {
        let player_clock_ns = self.0.elapsed_ns();
        match anchor {
            Some(a) => a.map(frame_pts_ns, player_clock_ns),
            None => player_clock_ns,
        }
    }

    fn flush(&self) -> anyhow::Result<()> {
        self.0.flush()
    }

    pub fn set_color_info(&self, meta: Option<&quic::HdrMeta>, color: quic::ColorInfo) -> anyhow::Result<()> {
        self.0.set_color_info(meta, color)
    }

    /// Whether the backend decodes audio itself.
    pub fn audio_offloaded(&self) -> bool {
        self.0.audio_offloaded()
    }

    /// Shared NDL handle when audio-offloaded; None on a video-only load.
    pub fn ndl_audio_handle(&self) -> Option<Arc<NdlVideo>> {
        self.0.audio_offloaded().then(|| self.0.clone())
    }

    /// NDL render-buffer backlog (None if the query fails).
    fn render_buffer_length(&self) -> Option<i32> {
        self.0.render_buffer_length()
    }
}

/// Everything protocol-specific the sink needs to know up front.
pub struct SinkConfig {
    /// The host's frame cadence. Drives both the pacing grid and the backlog→latency fold.
    pub stream_hz: u32,
    /// Whether the host asked for decode-latency reports (its ABR controller).
    pub report_decode_latency: bool,
    /// Minimum spacing between [`SinkResult::NeedKeyframe`] results — see
    /// each protocol's own constant for why this is per-protocol.
    pub keyframe_min_interval: Duration,
}

/// The NDL implementation: [`VideoPlayer`] + [`PtsPacer`] + [`StreamStats`].
pub struct NdlSink {
    player: VideoPlayer,
    stats: Arc<StreamStats>,
    cfg: SinkConfig,
    frame_period_us: u64,
    /// Always instantiated — the Blue button can flip pacing on mid-stream, so the state
    /// must exist even when it starts off. Pacer interval reconciles against the panel's
    /// measured refresh; the backlog fold stays on the stream rate (host's actual cadence).
    pacer: PtsPacer,
    /// NDL host-PTS→player-clock mapping, reset in lockstep with the pacer.
    host_anchor: HostPtsAnchor,
    /// Previous-frame pacing state, so an off→on flip can re-anchor cleanly.
    pacing_was_on: bool,
    backlog_cached: u64,
    last_backlog_poll: Option<Instant>,
    last_keyframe_request: Option<Instant>,
    /// Freeze-until-reanchor: while holding, frames are skipped rather than fed — the
    /// punch-through plane keeps the last good picture. Resumes on IDR / LTR-RFI recovery
    /// anchor, or after [`HOLD_GIVE_UP`]. `Some` for exactly as long as the hold lasts (see
    /// [`Self::holding`]), and not reset on cascading gaps so the give-up deadline can't be
    /// pushed out indefinitely.
    hold_started: Option<Instant>,
}

impl NdlSink {
    pub fn new(player: VideoPlayer, stats: Arc<StreamStats>, cfg: SinkConfig) -> Self {
        let stream_hz = cfg.stream_hz.max(1);
        let pacing_was_on = stats.pacing_enabled.load(Ordering::Relaxed);
        Self {
            player,
            stats,
            frame_period_us: 1_000_000 / u64::from(stream_hz),
            pacer: PtsPacer::new(reconciled_pace_interval_ns(stream_hz)),
            host_anchor: HostPtsAnchor::new(),
            pacing_was_on,
            cfg,
            backlog_cached: 0,
            last_backlog_poll: None,
            last_keyframe_request: None,
            hold_started: None,
        }
    }

    pub fn set_color_info(&self, meta: Option<&quic::HdrMeta>, color: quic::ColorInfo) -> anyhow::Result<()> {
        self.player.set_color_info(meta, color)
    }

    /// True when the throttle allows a keyframe request now; stamps it as sent.
    fn take_keyframe_slot(&mut self) -> bool {
        let ready = self
            .last_keyframe_request
            .is_none_or(|t| t.elapsed() >= self.cfg.keyframe_min_interval);
        if ready {
            self.last_keyframe_request = Some(Instant::now());
        }
        ready
    }

    fn begin_hold(&mut self) {
        self.stats.holding.store(true, Ordering::Relaxed);
        self.hold_started.get_or_insert_with(Instant::now);
    }

    /// The decode figure reported to the host's ABR controller. NDL's `play` is
    /// decode-AND-present in one opaque call, so `feed_elapsed` alone is *submission*
    /// time — a decoder quietly falling behind buffers frames internally and the feed
    /// stays fast, which left the controller's decode-rise signal (`abr::DECODE_RISE_US`,
    /// built precisely for "the decoder saturates before the link does") effectively
    /// blind on this client. The render-buffer backlog IS that standing decode queue, so
    /// it's folded in as `backlog × frame_period`. Polled on a cadence rather than every
    /// frame — three samples per 750 ms ABR report window is plenty, and assuming an NDL
    /// query is cheap enough for per-frame use is exactly the mistake docs/NOTES.md warns
    /// against; between polls the cached depth is reused.
    fn decode_us(&mut self, feed_elapsed: Duration) -> u32 {
        if self.last_backlog_poll.is_none_or(|t| t.elapsed() >= BACKLOG_POLL) {
            self.last_backlog_poll = Some(Instant::now());
            self.backlog_cached = self
                .player
                .render_buffer_length()
                .and_then(|b| u64::try_from(b).ok())
                .unwrap_or(0);
        }
        let decode_us = u64::try_from(feed_elapsed.as_micros())
            .unwrap_or(u64::MAX)
            .saturating_add(self.backlog_cached.saturating_mul(self.frame_period_us));
        u32::try_from(decode_us).unwrap_or(u32::MAX)
    }

    /// Whether a freeze-until-reanchor hold is currently active (stats/logging).
    pub fn holding(&self) -> bool {
        self.hold_started.is_some()
    }

    /// Decoder backlog depth, or `None` if the backend can't report one.
    pub fn backlog(&self) -> Option<i32> {
        self.player.render_buffer_length()
    }

    /// Present one access unit, or decide not to. `pts_ns` is the host's capture-clock
    /// PTS; the sink maps and paces it into the decoder's own clock domain.
    pub fn submit(&mut self, au: &[u8], pts_ns: u64, flags: FrameFlags) -> SinkResult {
        if flags.loss && !self.holding() {
            self.begin_hold();
            tracing::warn!("loss (frame {}) — freezing", flags.index);
            let _ = self.player.flush();
        }
        let mut need_keyframe = self.holding() && self.take_keyframe_slot();

        let gave_up = self.hold_started.is_some_and(|t| t.elapsed() >= HOLD_GIVE_UP);
        if self.holding() && !flags.reanchor && !gave_up {
            return if need_keyframe {
                SinkResult::NeedKeyframe
            } else {
                SinkResult::Held
            };
        }
        if self.holding() {
            tracing::info!(
                "resuming after {:.0}ms (frame {}, reanchor={}, gave_up={gave_up})",
                self.hold_started.map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0),
                flags.index,
                flags.reanchor,
            );
            // The real timeline just jumped (freeze then reanchor/give-up) — nothing about
            // the pre-hold accumulator is worth continuing.
            self.pacer.reset();
            self.host_anchor.reset();
        }
        self.stats.holding.store(false, Ordering::Relaxed);
        self.hold_started = None;

        // Live pacing toggle (Blue button): re-anchor on the off→on edge so the pacer picks
        // up from the current frame rather than a stale grid.
        let pacing_on = self.stats.pacing_enabled.load(Ordering::Relaxed);
        if pacing_on && !self.pacing_was_on {
            self.pacer.reset();
            self.host_anchor.reset();
        }
        self.pacing_was_on = pacing_on;

        let base_ns = self
            .player
            .pace_base_ns(pts_ns, pacing_on.then_some(&mut self.host_anchor));
        let paced_ns = if pacing_on {
            let paced = self.pacer.next(base_ns);
            self.stats
                .pacing_delta_ns
                .store(paced as i64 - base_ns as i64, Ordering::Relaxed);
            paced
        } else {
            self.stats.pacing_delta_ns.store(0, Ordering::Relaxed);
            base_ns
        };

        let (play_result, feed_elapsed) = self.player.play(au, paced_ns);
        self.stats.feed_us.store(
            u32::try_from(feed_elapsed.as_micros()).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
        if feed_elapsed >= FEED_BACKPRESSURE_WARN {
            tracing::warn!(
                "NDL slow: {:.1}ms (frame {}, pts {:.2}ms)",
                feed_elapsed.as_secs_f32() * 1000.0,
                flags.index,
                paced_ns as f64 / 1_000_000.0,
            );
        }

        let decode_us = (self.cfg.report_decode_latency && play_result.is_ok()).then(|| self.decode_us(feed_elapsed));

        if let Err(e) = play_result {
            tracing::warn!(
                "NDL error (frame {}, pts {:.2}ms): {e:#}",
                flags.index,
                paced_ns as f64 / 1_000_000.0,
            );
            if self.take_keyframe_slot() {
                let _ = self.player.flush();
                self.begin_hold();
                need_keyframe = true;
            }
        }

        if need_keyframe {
            SinkResult::NeedKeyframe
        } else {
            SinkResult::Presented { decode_us }
        }
    }
}
