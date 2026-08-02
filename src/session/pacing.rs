//! PTS pacing for the video pump: smooths the timestamps fed to the decoder against
//! delivery jitter so a burst of frames landing together is spaced one interval apart
//! rather than stamped with ~the same time. Ports aurora-tv's `SS4S_SMOOTH_PACING`
//! stack (`ndl_player.c`) — reconciled interval, host-PTS anchoring, and drift-clamped
//! smoothing. Enabled only when the frame-pacing setting is on (see [`crate::session`]).

// Read-only panel-refresh query from the `webosbrew/SDL-webOS` fork.
#[link(name = "SDL2")]
extern "C" {
    fn SDL_webOSGetRefreshRate(rate: *mut std::os::raw::c_int) -> std::os::raw::c_int;
}

/// Returns panel refresh in Hz, or `None` on query failure/implausible values.
fn panel_refresh_hz() -> Option<u32> {
    let mut rate: std::os::raw::c_int = 0;
    // SAFETY: single out-param, no aliasing; read-only panel query.
    let ok = unsafe { SDL_webOSGetRefreshRate(&mut rate) };
    (ok != 0 && (20..=240).contains(&rate)).then_some(rate as u32)
}

/// Pacing interval (ns): panel cadence if within ±2 Hz of stream, else stream rate.
pub fn reconciled_pace_interval_ns(stream_hz: u32) -> u64 {
    let hz = match panel_refresh_hz() {
        Some(panel_hz) if stream_hz.abs_diff(panel_hz) <= 2 => {
            tracing::info!("pacing grid anchored to panel {panel_hz}Hz (stream {stream_hz}Hz)");
            panel_hz
        }
        Some(panel_hz) => {
            tracing::info!("pacing grid on stream {stream_hz}Hz (panel {panel_hz}Hz differs by >2Hz)");
            stream_hz
        }
        None => stream_hz,
    };
    1_000_000_000 / u64::from(hz.max(1))
}

/// Max drift ([`PtsPacer`]) from the unpaced reference, as a fraction of one frame
/// interval — same figure aurora-tv ships as `SS4S_SMOOTH_PACING_MAX_DRIFT_FRAMES` for
/// this same NDL backend.
const PACE_MAX_DRIFT_FRAMES: f64 = 0.5;
/// Minimum step between successive paced PTS values, so NDL's ms-truncation
/// (`NdlVideo::play`) never sees two equal/decreasing timestamps in a row.
const PACE_MIN_STEP_NS: u64 = 1_000_000;

/// Maps the host's capture-clock PTS onto NDL's own player clock, anchored once at the
/// first frame of a run: `base = player_anchor + (host_pts - host_anchor)`. Keeps the
/// pacer's drift-clamp reference on host frame cadence instead of feed-time wall-clock,
/// so network jitter doesn't leak into the clamp target (see
/// [`crate::session::VideoPlayer::pace_base_ns`]). Same anchoring as SS4S's
/// `ndl_player.c::SS4S_NDL_webOS5_NextVideoPts`. Reset alongside [`PtsPacer`] after a
/// freeze-until-reanchor hold, where both timelines jump.
pub struct HostPtsAnchor {
    anchor: Option<(u64, u64)>,
}

impl HostPtsAnchor {
    pub fn new() -> Self {
        Self { anchor: None }
    }

    pub fn reset(&mut self) {
        self.anchor = None;
    }

    /// Base reference for `host_pts_ns`. First call anchors on `player_clock_ns` and
    /// returns it verbatim; later calls project the host PTS delta forward, floored at 0
    /// (a host PTS going backwards vs. the anchor would otherwise underflow).
    pub fn map(&mut self, host_pts_ns: u64, player_clock_ns: u64) -> u64 {
        match self.anchor {
            None => {
                self.anchor = Some((host_pts_ns, player_clock_ns));
                player_clock_ns
            }
            Some((host0, player0)) => {
                let delta = host_pts_ns as i64 - host0 as i64;
                (player0 as i64 + delta).max(0) as u64
            }
        }
    }
}

/// Smooths the PTS fed to the decoder against delivery jitter — an "ideal" value
/// advances by a fixed frame interval each call, clamped to a small drift window around
/// the real (unpaced) reference (see [`crate::session::VideoPlayer::pace_base_ns`]).
/// Changes only *what* timestamp is attached, never *when* `play()` runs, so a burst of
/// frames landing together still gets spaced one interval apart. Same technique as
/// aurora-tv's `SS4S_SMOOTH_PACING`.
pub struct PtsPacer {
    interval_ns: u64,
    max_drift_ns: u64,
    ideal_ns: Option<u64>,
}

impl PtsPacer {
    pub fn new(interval_ns: u64) -> Self {
        Self {
            interval_ns,
            max_drift_ns: (interval_ns as f64 * PACE_MAX_DRIFT_FRAMES) as u64,
            ideal_ns: None,
        }
    }

    /// Drops the accumulator — call after a freeze-until-reanchor hold, where the real
    /// timeline just jumped and there's no "ideal" continuation worth preserving.
    pub fn reset(&mut self) {
        self.ideal_ns = None;
    }

    /// Next paced PTS (ns) for `base_ns`, this frame's unpaced reference value. First
    /// call anchors on `base_ns` verbatim; later calls advance one interval from the
    /// previous paced value, clamped to the drift window and floored to a
    /// strictly-increasing step.
    pub fn next(&mut self, base_ns: u64) -> u64 {
        let paced = match self.ideal_ns {
            None => base_ns,
            Some(prev) => prev
                .saturating_add(self.interval_ns)
                .clamp(
                    base_ns.saturating_sub(self.max_drift_ns),
                    base_ns.saturating_add(self.max_drift_ns),
                )
                .max(prev.saturating_add(PACE_MIN_STEP_NS)),
        };
        self.ideal_ns = Some(paced);
        paced
    }
}
