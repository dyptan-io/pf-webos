//! Connects to a punktfunk host and drives the video/audio hardware pipelines.
//!
//! Video runs on a dedicated thread ([`video_pump`]) behind a [`VideoPlayer`] abstraction
//! over the NDL and Starfish backends.
//!
//! Audio is drained from the main thread ([`pump_audio_once`]) because
//! `sdl2::audio::AudioQueue` is `!Send`.
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::client::{NativeClient, ProbeOutcome};
use punktfunk_core::config::{CompositorPref, Mode};
use punktfunk_core::input::InputEvent;
use punktfunk_core::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR};
use punktfunk_core::quic;

use crate::ndl::{NdlCodec, NdlVideo};
use crate::starfish::StarfishVideo;
use crate::store::VideoBackend;

// ─────────────────────────────────────────────────────────── VideoPlayer ──

/// Unified video-decode backend, selected at connect time via [`VideoBackend`].
enum VideoPlayer {
    Starfish(StarfishVideo),
    Ndl(NdlVideo),
}

impl VideoPlayer {
    /// Feed one access unit. `pts_ns` is nanoseconds (`frame.pts_ns` from the host).
    /// Returns the feed duration for ABR decode-latency reporting.
    fn play(&self, au: &[u8], pts_ns: u64) -> (anyhow::Result<()>, Duration) {
        let t = Instant::now();
        let result = match self {
            Self::Starfish(sf) => sf.play(au, pts_ns),
            Self::Ndl(ndl) => ndl.play(au),
        };
        (result, t.elapsed())
    }

    fn flush(&self) -> anyhow::Result<()> {
        match self {
            Self::Starfish(sf) => sf.flush(),
            Self::Ndl(ndl) => ndl.flush(),
        }
    }

    fn set_color_info(&self, meta: Option<&quic::HdrMeta>, color: quic::ColorInfo) -> anyhow::Result<()> {
        match self {
            Self::Starfish(sf) => sf.set_color_info(meta, color),
            Self::Ndl(ndl) => ndl.set_color_info(meta, color),
        }
    }

    /// Whether the active backend is decoding audio itself (NDL only — the Starfish
    /// payload sets `needAudio: false`, so that path always uses the software decoder).
    fn audio_offloaded(&self) -> bool {
        match self {
            Self::Ndl(ndl) => ndl.audio_offloaded(),
            Self::Starfish(_) => false,
        }
    }

    /// Hands one raw Opus packet to the backend. Only called when `audio_offloaded`.
    fn play_audio(&self, packet: &[u8]) -> anyhow::Result<()> {
        match self {
            Self::Ndl(ndl) => ndl.play_audio(packet),
            Self::Starfish(_) => Ok(()),
        }
    }

    /// NDL's undecoded/unpresented backlog, when the active backend can report it.
    /// Starfish exposes no equivalent through the wrapper, so it reports `None` — the
    /// overlay and the log then simply omit the figure rather than showing a fake zero.
    fn render_buffer_length(&self) -> Option<i32> {
        match self {
            Self::Ndl(ndl) => ndl.render_buffer_length(),
            Self::Starfish(_) => None,
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            Self::Starfish(_) => "Starfish/SMP",
            Self::Ndl(_) => "NDL",
        }
    }
}

pub struct Connected {
    pub client: Arc<NativeClient>,
    pub stop: Arc<AtomicBool>,
    /// Live pump counters for the stats overlay — see [`StreamStats`].
    pub stats: Arc<StreamStats>,
    /// Kept alive so [`Connected::shutdown`] can join it and ensure `NativeClient::Drop`
    /// (which sends the QUIC close frame) runs to completion before process exit.
    video_thread: std::thread::JoinHandle<()>,
    /// Set when NDL accepted the Opus config and is decoding audio itself, so the caller
    /// must NOT also open an SDL2 audio device — see `ndl_audio_config`.
    pub audio_offloaded: bool,
}

/// Live video-pump counters shared with the main thread for the in-stream stats
/// overlay (`Settings::stats_overlay`): plain relaxed atomics, written per frame
/// by [`video_pump`], read at the overlay's ~2Hz refresh. Dropped-frame counts
/// come straight from `NativeClient::frames_dropped()` at read time instead.
#[derive(Default)]
pub struct StreamStats {
    /// Total frames received from the host so far.
    pub frames: std::sync::atomic::AtomicU64,
    /// Whether the freeze-until-reanchor hold is currently active.
    pub holding: AtomicBool,
    /// The most recent decoder feed duration, in µs.
    pub feed_us: std::sync::atomic::AtomicU32,
    /// NDL's render-buffer backlog at the last heartbeat, or `-1` when the active
    /// backend can't report one (see `VideoPlayer::render_buffer_length`).
    pub render_backlog: std::sync::atomic::AtomicI32,
}

/// Short display name for a resolved wire codec id (the stats overlay's header).
pub fn codec_name(codec: u8) -> &'static str {
    match codec {
        c if c == quic::CODEC_HEVC => "HEVC",
        c if c == quic::CODEC_H264 => "H264",
        c if c == quic::CODEC_AV1 => "AV1",
        _ => "?",
    }
}

impl Connected {
    /// Stops and joins the video thread, then drops the `NativeClient` reference.
    ///
    /// Call `self.client.disconnect_quit()` before this for a deliberate stop
    /// (app quit, long-press Back); omit it when the host ended the session.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.video_thread.join();
        drop(self.client);
    }
}

/// Whether to ask NDL to decode the audio stream, given the host-resolved channel count.
///
/// **Stereo only, by construction.** `NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T` carries a channel
/// count and a sample rate and nothing else — it has no multistream mapping field, so it
/// cannot describe the 5.1/7.1 layouts `punktfunk_core::audio::layout_for` negotiates
/// (those are Opus multistream, with a per-layout stream/coupled/mapping triple). Handing
/// NDL a `channels: 6` it would decode as plain 6-channel Opus would produce noise, not
/// surround, so anything above stereo stays on the software decoder.
///
/// Whether the *device* implements the Opus path at all is a separate question, answered
/// by probe rather than assumption — see `NdlVideo::load`.
fn ndl_audio_config(resolved_channels: u8) -> Option<crate::ndl::NdlAudioConfig> {
    (resolved_channels == 2).then_some(crate::ndl::NdlAudioConfig {
        channels: 2,
        // punktfunk's audio plane is fixed at 48 kHz (see `audio.rs`'s SAMPLE_RATE).
        sample_rate: 48_000.0,
    })
}

/// Default HDR10 mastering metadata for the LG CX OLED panel.
/// Sent in `Hello::display_hdr`; refined per-content by `next_hdr_meta`.
fn cx_display_hdr() -> quic::HdrMeta {
    quic::HdrMeta {
        // G, B, R order (ST.2086), 1/50000 chromaticity units — BT.2020 primaries.
        display_primaries: [[8_500, 39_850], [6_550, 2_300], [35_400, 14_600]],
        white_point: [15_635, 16_450], // D65
        max_display_mastering_luminance: 800 * 10_000,
        min_display_mastering_luminance: 5,
        max_cll: 800,
        max_fall: 150,
    }
}

/// Connects to a punktfunk host and starts the video pump thread.
///
/// Blocks until the handshake completes or `timeout` elapses. `pin` is the trusted
/// host fingerprint from a prior pairing (`None` = trust-on-first-use). `display_w`
/// / `display_h` is the physical panel size for the Starfish punch-through window —
/// independent of `mode` (the negotiated stream resolution). NDL manages its own
/// punch-through area natively (see [`crate::ndl`]'s module docs).
#[allow(clippy::too_many_arguments)]
pub fn connect(
    host: &str,
    port: u16,
    mode: Mode,
    bitrate_kbps: u32,
    hdr_enabled: bool,
    audio_channels: u8,
    identity: (String, String),
    pin: Option<[u8; 32]>,
    launch: Option<String>,
    timeout: Duration,
    display_w: i32,
    display_h: i32,
    video_backend: VideoBackend,
) -> Result<Connected> {
    // VIDEO_CAP_CHACHA20: unconditional — armv7 has no hardware AES, so ChaCha20 is
    // faster. A ≥0.17.2 host picks it up; older hosts ignore the unknown bit.
    let video_caps = quic::VIDEO_CAP_CHACHA20
        | if hdr_enabled {
            quic::VIDEO_CAP_10BIT | quic::VIDEO_CAP_HDR
        } else {
            0
        };
    let display_hdr = hdr_enabled.then(cx_display_hdr);

    let client = NativeClient::connect(
        host,
        port,
        mode,
        CompositorPref::Auto,
        punktfunk_core::config::GamepadPref::Auto,
        bitrate_kbps,
        video_caps,
        // Requested only — the host clamps to what it can capture, and
        // `AudioPlayer::new` is built from the RESOLVED `client.audio_channels`,
        // never from this.
        audio_channels,
        quic::CODEC_HEVC | quic::CODEC_H264,
        0, // let the host choose
        display_hdr,
        0, // client_caps: this client composites the host cursor into the video, not locally
        launch,
        pin,
        Some(identity),
        timeout,
    )
    .context("connect")?;
    let client = Arc::new(client);

    let fp_hex = client.host_fingerprint.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    tracing::info!(
        "connected: codec={} compositor={:?} audio_ch={} color={:?} bitrate_kbps={} \
         decode_latency={} caps=0x{video_caps:02x} fp={fp_hex}",
        client.codec,
        client.resolved_compositor,
        client.audio_channels,
        client.color,
        client.resolved_bitrate_kbps,
        client.wants_decode_latency(),
    );

    let resolved_mode = client.mode();
    let fps = resolved_mode.refresh_hz.max(1);
    let codec =
        NdlCodec::from_wire(client.codec).with_context(|| format!("unsupported codec 0x{:02x}", client.codec))?;
    let app_id = std::env::var("APPID").unwrap_or_else(|_| "io.dyptan.punktfunk.webos".into());

    let player = match video_backend {
        VideoBackend::Starfish => {
            // `StarfishVideo::load` documents failure (its wrapper `.so` absent /
            // the service refusing the load) as "caller falls back to NDL" — honor
            // that instead of propagating, which would take the whole app down on
            // every launch until the user remembered to flip the setting back.
            match StarfishVideo::load(
                &app_id,
                resolved_mode.width as i32,
                resolved_mode.height as i32,
                fps,
                codec,
                display_w,
                display_h,
            ) {
                Ok(sf) => {
                    tracing::info!(
                        "Starfish loaded ({codec:?} {}x{}@{fps}fps, display {display_w}x{display_h})",
                        resolved_mode.width,
                        resolved_mode.height,
                    );
                    VideoPlayer::Starfish(sf)
                }
                Err(e) => {
                    tracing::warn!("Starfish load failed ({e:#}) — falling back to NDL");
                    let ndl = NdlVideo::load(
                        &app_id,
                        resolved_mode.width as i32,
                        resolved_mode.height as i32,
                        codec,
                        ndl_audio_config(client.audio_channels),
                    )
                    .context("NDL load (Starfish fallback)")?;
                    tracing::info!(
                        "NDL loaded ({codec:?} {}x{}@{fps}fps)",
                        resolved_mode.width,
                        resolved_mode.height,
                    );
                    VideoPlayer::Ndl(ndl)
                }
            }
        }
        VideoBackend::Ndl => {
            let ndl = NdlVideo::load(
                &app_id,
                resolved_mode.width as i32,
                resolved_mode.height as i32,
                codec,
                ndl_audio_config(client.audio_channels),
            )
            .context("NDL load")?;
            tracing::info!(
                "NDL loaded ({codec:?} {}x{}@{fps}fps)",
                resolved_mode.width,
                resolved_mode.height,
            );
            VideoPlayer::Ndl(ndl)
        }
    };

    // An on-device sweep value for NDL's undocumented frame-drop threshold, if one has
    // been dropped in (see `store::dev_override_ndl_drop_threshold`). Never set by
    // default — the units aren't documented and a guessed pacing change to this decoder
    // is exactly what `docs/NOTES.md` warns against shipping unverified.
    if matches!(video_backend, VideoBackend::Ndl) {
        if let Some(threshold) = crate::store::dev_override_ndl_drop_threshold() {
            match crate::ndl::NdlVideo::set_frame_drop_threshold(threshold) {
                Ok(()) => tracing::info!("NDL frame-drop threshold override: {threshold}"),
                Err(e) => tracing::warn!("NDL frame-drop threshold override failed: {e:#}"),
            }
        }
    }

    // Forward the negotiated colorimetry to the decoder for BOTH HDR and SDR
    // streams. The SDR case is not optional: punktfunk encodes BT.709, but with
    // missing/"unspecified" VUI colour info in the bitstream this panel guesses
    // colorimetry from resolution — a 4K SDR stream then decodes as BT.2020,
    // which shows up as exactly the washed-out/desaturated picture reported
    // on-device. `client.color` arrives out-of-band in `Welcome` for precisely
    // this purpose; HDR streams additionally carry mastering metadata.
    let is_hdr = client.color.is_hdr();
    let initial_meta = is_hdr.then(cx_display_hdr);
    if let Err(e) = player.set_color_info(initial_meta.as_ref(), client.color) {
        tracing::warn!("{} colour metadata failed: {e:#}", player.backend_name());
    }
    tracing::debug!(
        "colour metadata sent: hdr={is_hdr} transfer={} primaries={} matrix={} full_range={}",
        client.color.transfer,
        client.color.primaries,
        client.color.matrix,
        client.color.full_range,
    );

    let audio_offloaded = player.audio_offloaded();
    tracing::info!(
        "audio path: {} (host resolved {} channel(s))",
        if audio_offloaded {
            "NDL hardware Opus decode"
        } else {
            "software Opus decode -> SDL2"
        },
        client.audio_channels,
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StreamStats::default());
    let video_client = client.clone();
    let video_stop = stop.clone();
    let video_stats = stats.clone();
    let video_thread = std::thread::Builder::new()
        .name("punktfunk-webos-video".into())
        .spawn(move || video_pump(video_client, player, video_stop, video_stats, is_hdr, audio_offloaded))
        .context("spawn video thread")?;

    Ok(Connected {
        client,
        stop,
        stats,
        video_thread,
        audio_offloaded,
    })
}

/// The no-PIN "request access" trust step: open a trust-on-first-use connection
/// (`pin = None`) presenting our identity, which a host requiring pairing PARKS until
/// its operator approves this device, then return the host's now-verified fingerprint
/// to pin and tear the connection straight back down.
///
/// Uses [`NativeClient`] directly rather than [`connect`] above: no video backend
/// (NDL/Starfish) is loaded and no pump thread is spawned, so the video plane is never
/// touched — this only needs the handshake to reach `Welcome`, not a running stream. The
/// negotiated `mode`/codec are irrelevant here (immediately dropped); a small 720p H.264
/// request keeps the host from doing needless 4K/HEVC setup for a connection we close at
/// once. Blocks up to `timeout` (the operator-approval window).
pub fn request_access(host: &str, port: u16, identity: (String, String), timeout: Duration) -> Result<[u8; 32]> {
    let mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let client = NativeClient::connect(
        host,
        port,
        mode,
        CompositorPref::Auto,
        punktfunk_core::config::GamepadPref::Auto,
        1_000, // minimal bitrate — connection is closed as soon as trust is established
        quic::VIDEO_CAP_CHACHA20,
        2,
        quic::CODEC_H264,
        0,
        None, // no HDR display metadata
        0,    // client_caps: no local cursor rendering
        None, // no launch
        None, // pin = None → trust-on-first-use, host parks until operator approval
        Some(identity),
        timeout,
    )
    .context("request access connect")?;
    let fingerprint = client.host_fingerprint;
    // Deliberate teardown — the host should drop the parked/approved session now, not
    // linger for a stream that isn't coming. (Runs on a background thread — see
    // `App::try_request_access` — so no log handle here; the caller logs the outcome.)
    client.disconnect_quit();
    Ok(fingerprint)
}

/// What the host is asked to burst during a speed test, and for how long.
///
/// Deliberately **not** the other clients' 3 Gbps / 5 s, and not the 1 Gbps this first
/// shipped with either. Measured on a real CX over Wi-Fi against a 0.19.2 host: a 1 Gbps
/// request was honoured exactly (375 MB pushed in 3 s) while the TV received 87 MB —
/// **~80 % loss** — and in half the attempts the host's end-of-burst `ProbeResult`, which
/// travels over the QUIC control stream *through that same saturated path*, never arrived
/// at all. Overshooting capacity is how a probe finds a ceiling, but overshooting it
/// fourfold mostly measures the access point's drop policy and costs the measurement its
/// own result message.
///
/// The target is chosen against what the answer can actually be *used* for: the bitrate
/// slider caps at `ui::BITRATE_MAX_KBPS` (200 Mbps) and the recommendation is 70 % of
/// measured, so anything above ~285 Mbps already produces an identical clamped
/// recommendation. 320 Mbps stays above that — it can still detect any ceiling that
/// would change the advice — while keeping the overshoot bounded. Measured on a G5
/// (2026-07-24, warm data plane, sweep 260/280/320/400): delivered goodput is a flat
/// ~245 Mbps at every offered rate — the TV's own Wi-Fi radio (USB 2.0-attached) is the
/// ceiling, independently confirmed with a raw UDP flood — so a 400 Mbps burst just
/// raises the shed overshoot (51 % packet loss vs 38 % at 320) and with it the odds the
/// end-of-burst report is starved out, for zero extra information.
const PROBE_TARGET_KBPS: u32 = 320_000;
/// A pinned (non-zero) session rate for the probe connect — see the call site: its only
/// job is to keep `bitrate_kbps == 0` from arming core's own capacity probe against the
/// single shared `ProbeState`. Nothing decodes here, so the value itself is immaterial.
const PROBE_SESSION_BITRATE_KBPS: u32 = 20_000;
/// Below this many delivered bytes, a missing host report is a failure rather than
/// something to salvage — 1 MB over a 3 s burst is ~2.7 Mbps, far under anything worth
/// recommending a bitrate from.
const SALVAGE_MIN_BYTES: u64 = 1024 * 1024;
const PROBE_DURATION_MS: u32 = 3_000;
/// How long to wait for the data plane to prove itself live (first completed video frame)
/// before bursting. Observed on the G5 over Wi-Fi (2026-07-24): a NEW host→client UDP flow
/// is sometimes black-holed (AP/driver flow setup — even the session's own 20 Mbps video
/// is held, while QUIC control chats at ~1 ms RTT), then dumped all at once. Measured
/// holes ranged ~10-29 s, longer after longer idle. A burst fired into that window
/// measures the black hole, not the link. Waiting for the first delivered frame starts
/// every measurement on a proven-live plane; if nothing arrives within the cap, proceed
/// anyway — the burst then behaves exactly as before.
const PROBE_WARMUP_CAP: Duration = Duration::from_secs(35);
/// How long to keep polling for the host's end-of-burst report after the burst should
/// have finished before giving up. Generous: the report shares a link the burst has just
/// been hammering, so its first delivery attempt can well be lost and need a retransmit.
const PROBE_REPORT_GRACE: Duration = Duration::from_secs(12);

/// A finished speed test, and whether the host confirmed the figures.
pub struct SpeedProbeResult {
    pub outcome: ProbeOutcome,
    /// `true` when the host's end-of-burst report arrived. `false` means it never did and
    /// the throughput was derived from what this client actually received over the burst
    /// window it asked for — a real measurement, but with no host-side cross-check, so no
    /// loss figure and a conservative reading if the burst was cut short.
    pub confirmed: bool,
}

/// Runs one network speed test against `host` and returns the host's final measurement.
///
/// Like [`request_access`], this uses [`NativeClient`] directly rather than [`connect`]:
/// no video backend is loaded and no pump thread is spawned, so the punch-through plane
/// is never touched — the host builds a virtual output, but nothing is decoded or
/// presented. Blocks; run it on a worker thread.
///
/// **`video_caps` must advertise `VIDEO_CAP_CHACHA20` exactly as a real session does.**
/// `punktfunk-core` counts the delivered bytes this measurement is derived from *after*
/// AEAD decrypt, so a probe that negotiated AES-GCM would measure a ceiling this armv7
/// CPU can't reach with the cipher an actual stream uses — reporting a number no session
/// could ever deliver. See `docs/NOTES.md` on why `ChaCha20` exists on this client at all.
///
/// `progress` is called with each partial poll so the UI can show the figure climbing.
pub fn run_speed_probe(
    host: &str,
    port: u16,
    identity: (String, String),
    pin: Option<[u8; 32]>,
    timeout: Duration,
    mut progress: impl FnMut(ProbeOutcome),
) -> Result<SpeedProbeResult> {
    let mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    let client = NativeClient::connect(
        host,
        port,
        mode,
        CompositorPref::Auto,
        punktfunk_core::config::GamepadPref::Auto,
        // NOT 0. `bitrate_kbps == 0` is what arms punktfunk-core's OWN startup
        // link-capacity probe (`client/pump/data.rs`: 2 Gbps for 800ms, ~2s after
        // connect) — and core has exactly one `ProbeState` slot with no correlation id,
        // which our `request_probe` below would be sharing with it. Core defers its
        // probe while ours is active, but the reverse race (its probe landing just as
        // ours finishes and resetting the state we're about to read) is real. Pinning a
        // rate disarms core's probe entirely; the value is irrelevant since nothing is
        // decoded here.
        PROBE_SESSION_BITRATE_KBPS,
        quic::VIDEO_CAP_CHACHA20,
        2, // stereo baseline
        quic::CODEC_HEVC | quic::CODEC_H264,
        0,    // no preferred codec
        None, // no HDR display metadata: nothing presents
        0,    // client_caps: nothing renders a cursor
        None, // no launch
        pin,
        Some(identity),
        timeout,
    )
    .context("speed test connect")?;

    // The negotiated session, logged before the burst: if a measurement comes back
    // empty, this line is what says whether the connection itself was sane.
    tracing::info!(
        "speed test connected: codec={} audio_ch={} resolved_bitrate_kbps={} caps=0x{:02x}",
        client.codec,
        client.audio_channels,
        client.resolved_bitrate_kbps,
        quic::VIDEO_CAP_CHACHA20,
    );

    // Don't burst into a dead plane — see PROBE_WARMUP_CAP. `next_frame` drains the session's
    // decode-less video into the void; the first completed frame is the "plane is live" edge.
    let warmup = Instant::now();
    let mut warmed = false;
    while warmup.elapsed() < PROBE_WARMUP_CAP {
        if client.next_frame(Duration::from_millis(250)).is_ok() {
            warmed = true;
            break;
        }
    }
    tracing::info!(
        "speed test: data plane {} after {} ms",
        if warmed {
            "live"
        } else {
            "still silent (proceeding anyway)"
        },
        warmup.elapsed().as_millis(),
    );

    client
        .request_probe(PROBE_TARGET_KBPS, PROBE_DURATION_MS)
        .context("request_probe")?;
    // Flip the UI from "Connecting…" to "Measuring…" the moment the burst is requested —
    // with the warmup above, the first 250 ms poll is no longer the earliest signal.
    progress(client.probe_result());

    let deadline = Instant::now() + Duration::from_millis(u64::from(PROBE_DURATION_MS)) + PROBE_REPORT_GRACE;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let outcome = client.probe_result();
        if outcome.done {
            // Let the last in-flight UDP shards land before tearing the connection
            // down, so the delivered-bytes figure isn't cut short by our own exit.
            std::thread::sleep(Duration::from_millis(400));
            let final_outcome = client.probe_result();
            // Both sides of the measurement, separately. This is the line that tells a
            // host-side problem from a client-side one: `host_bytes == 0` means the host
            // never put filler on the wire (it ignored or couldn't serve the request),
            // whereas `host_bytes > 0` with `recv_bytes == 0` means it sent and we
            // received nothing usable — a network path or a decrypt mismatch, since
            // punktfunk-core counts bytes only AFTER a successful AEAD open.
            tracing::info!(
                "speed test result: recv_bytes={} recv_packets={} host_bytes={} host_packets={} \
                 elapsed_ms={} throughput_kbps={} loss_pct={:.2} host_drop_pct={:.2} \
                 wire_packets_sent={} send_dropped={}",
                final_outcome.recv_bytes,
                final_outcome.recv_packets,
                final_outcome.host_bytes,
                final_outcome.host_packets,
                final_outcome.elapsed_ms,
                final_outcome.throughput_kbps,
                final_outcome.loss_pct,
                final_outcome.host_drop_pct,
                final_outcome.wire_packets_sent,
                final_outcome.send_dropped,
            );
            client.disconnect_quit();
            return Ok(SpeedProbeResult {
                outcome: final_outcome,
                confirmed: true,
            });
        }
        progress(outcome);
        if Instant::now() > deadline {
            // The report never came — but `recv_bytes` is live during the burst (core
            // computes it as `rx_now - base`), so if a real amount of filler arrived the
            // measurement is not lost: divide it by the burst window we asked for. The
            // host honours that duration exactly when it does report (confirmed
            // on-device: a 3,000 ms request came back as `elapsed_ms=3000`), so this is
            // the same denominator, just not host-attested. Only the loss figure is
            // genuinely unavailable, since that needs the host's sent-packet count.
            let mut salvaged = client.probe_result();
            client.disconnect_quit();
            if salvaged.recv_bytes >= SALVAGE_MIN_BYTES {
                salvaged.elapsed_ms = PROBE_DURATION_MS;
                salvaged.throughput_kbps =
                    (salvaged.recv_bytes.saturating_mul(8) / u64::from(PROBE_DURATION_MS)) as u32;
                tracing::warn!(
                    "speed test: no host report; salvaged from {} received bytes over the {} ms \
                     burst window -> {} kbps (unconfirmed)",
                    salvaged.recv_bytes,
                    PROBE_DURATION_MS,
                    salvaged.throughput_kbps,
                );
                return Ok(SpeedProbeResult {
                    outcome: salvaged,
                    confirmed: false,
                });
            }
            anyhow::bail!(
                "the host never sent its result, and almost nothing arrived. The test burst can \
                 saturate the link the result has to come back over — try again, or move the TV \
                 closer to the access point."
            );
        }
    }
}

/// Throttle for keyframe requests during hold or decode errors.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Freeze duration after which we resume even without a clean re-anchor.
const HOLD_GIVE_UP: Duration = Duration::from_secs(2);
/// Feed calls slower than this suggest decoder backpressure rather than network loss.
const FEED_BACKPRESSURE_WARN: Duration = Duration::from_millis(20);

/// Suffix identifying a `GStreamer` pad-task thread (`"<element-name>:<pad-name>"`,
/// truncated to the kernel's 15-char `comm` limit) — both the NDL and Starfish vendor
/// `.so`s build their internal decode pipeline out of `GStreamer` elements, each with its
/// own pad-task thread spawned *inside our own process*. These are invisible to
/// punktfunk-core's hot-thread registry (that only covers threads this crate and
/// punktfunk-core spawn themselves) and sit at the default nice 0 despite doing real
/// decode work — confirmed via live `/proc/<pid>/task` sampling during an active NDL
/// stream (its `lxvideodec1:src`/`video-src:src` threads), a real contention cost
/// against our own already-boosted video-pump/data-pump threads on this `SoC`'s 3 cores.
/// Matched by suffix, not a fixed name list, so this also covers whichever
/// differently-named elements the active backend's pipeline happens to use (e.g.
/// Starfish's own, not just the ones observed under NDL).
const VENDOR_DECODE_THREAD_SUFFIX: &str = ":src";
/// How long a decode-thread scan may run with no new match before concluding the
/// backend's pipeline has finished spawning threads (typically well under this in
/// practice). Bounded separately by `VENDOR_DECODE_THREAD_SCAN_TIMEOUT` in case a
/// backend never produces a matching thread at all.
const VENDOR_DECODE_THREAD_QUIET_PERIOD: Duration = Duration::from_millis(500);
const VENDOR_DECODE_THREAD_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

/// Renices the active backend's vendor-spawned `GStreamer` pad-task threads to -10, same
/// as this crate's own hot threads (see [`VENDOR_DECODE_THREAD_SUFFIX`]). Runs on its
/// own thread — these threads spawn asynchronously sometime after the decoder loads,
/// not synchronously within the load call, so this polls `/proc/self/task` rather than
/// scanning once, and must not block `video_pump` from starting to feed frames while it
/// does.
fn spawn_vendor_decode_thread_renicer() {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut last_found = start;
        let mut reniced: std::collections::HashSet<i32> = std::collections::HashSet::new();
        loop {
            if let Ok(entries) = std::fs::read_dir("/proc/self/task") {
                for entry in entries.flatten() {
                    let Ok(tid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                        continue;
                    };
                    if reniced.contains(&tid) {
                        continue;
                    }
                    let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
                        continue;
                    };
                    let comm = comm.trim();
                    if !comm.ends_with(VENDOR_DECODE_THREAD_SUFFIX) {
                        continue;
                    }
                    reniced.insert(tid);
                    last_found = Instant::now();
                    // SAFETY: plain syscall — tid and priority value only, no pointers.
                    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -10) } != 0 {
                        tracing::warn!(
                            "setpriority(vendor thread {comm}, tid={tid}) failed: {}",
                            std::io::Error::last_os_error()
                        );
                    } else {
                        tracing::debug!("reniced vendor decode thread {comm} (tid={tid}) to -10");
                    }
                }
            }
            let now = Instant::now();
            let quiet = !reniced.is_empty() && now.duration_since(last_found) >= VENDOR_DECODE_THREAD_QUIET_PERIOD;
            if quiet || now.duration_since(start) >= VENDOR_DECODE_THREAD_SCAN_TIMEOUT {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn video_pump(
    client: Arc<NativeClient>,
    player: VideoPlayer,
    stop: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
    is_hdr: bool,
    audio_offloaded: bool,
) {
    client.register_hot_thread();
    for tid in client.hot_thread_ids() {
        // SAFETY: plain syscall — tid and priority value only, no pointers.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -10) } != 0 {
            tracing::debug!(
                "setpriority(tid={tid}) failed (expected without CAP_SYS_NICE): {}",
                std::io::Error::last_os_error()
            );
        }
    }
    spawn_vendor_decode_thread_renicer();

    let wants_decode_latency = client.wants_decode_latency();
    let mut last_dropped_seen = client.frames_dropped();
    let mut last_keyframe_request: Option<Instant> = None;
    // Freeze-until-reanchor: while `holding`, frames are skipped rather than fed —
    // the punch-through plane keeps the last good picture. Resumes on IDR / LTR-RFI
    // recovery anchor, or after HOLD_GIVE_UP. `hold_started` is not reset on
    // cascading gaps so the give-up deadline can't be pushed out indefinitely.
    let mut holding = false;
    let mut hold_started: Option<Instant> = None;
    let mut frames_received: u64 = 0;
    let mut last_heartbeat = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match client.next_frame(Duration::from_millis(500)) {
            Ok(frame) => {
                frames_received += 1;
                stats.frames.store(frames_received, Ordering::Relaxed);
                if last_heartbeat.elapsed() >= Duration::from_secs(2) {
                    last_heartbeat = Instant::now();
                    let backlog = player.render_buffer_length();
                    stats.render_backlog.store(backlog.unwrap_or(-1), Ordering::Relaxed);
                    // `backlog` separates "the decoder is behind" from "frames are
                    // arriving late" — indistinguishable before this, since play()
                    // decodes and presents in one opaque call.
                    tracing::debug!(
                        "video: {frames_received} frames, holding={holding}, dropped={}, backlog={}",
                        client.frames_dropped(),
                        backlog.map_or_else(|| "n/a".to_string(), |b| b.to_string()),
                    );
                }

                let gap = client.note_frame_index(frame.frame_index);
                let dropped_now = client.frames_dropped();
                let dropped = dropped_now > last_dropped_seen;
                if dropped {
                    last_dropped_seen = dropped_now;
                }
                if (gap || dropped) && !holding {
                    holding = true;
                    stats.holding.store(true, Ordering::Relaxed);
                    hold_started = Some(Instant::now());
                    tracing::warn!(
                        "loss (gap={gap} dropped={dropped}, frame {}) — freezing",
                        frame.frame_index
                    );
                    let _ = player.flush();
                }
                if holding && last_keyframe_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL) {
                    if let Err(e) = client.request_keyframe() {
                        tracing::warn!("request_keyframe: {e:#}");
                    }
                    last_keyframe_request = Some(Instant::now());
                }

                let is_reanchor =
                    frame.flags & u32::from(FLAG_SOF) != 0 || frame.flags & USER_FLAG_RECOVERY_ANCHOR != 0;
                let gave_up = hold_started.is_some_and(|t| t.elapsed() >= HOLD_GIVE_UP);
                if holding && !is_reanchor && !gave_up {
                    // Still frozen — drop this concealed frame, but fall through to the
                    // HDR poll below instead of `continue`ing past it.
                } else {
                    if holding {
                        tracing::info!(
                            "resuming after {:.0}ms (frame {}, flags=0x{:x}, reanchor={is_reanchor}, gave_up={gave_up})",
                            hold_started.map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0),
                            frame.frame_index,
                            frame.flags,
                        );
                    }
                    holding = false;
                    stats.holding.store(false, Ordering::Relaxed);
                    hold_started = None;

                    let pts_ns = frame.pts_ns;
                    let (play_result, feed_elapsed) = player.play(&frame.data, pts_ns);
                    stats.feed_us.store(
                        u32::try_from(feed_elapsed.as_micros()).unwrap_or(u32::MAX),
                        Ordering::Relaxed,
                    );

                    if feed_elapsed >= FEED_BACKPRESSURE_WARN {
                        tracing::warn!(
                            "{} slow: {:.1}ms (frame {}, pts {:.2}ms)",
                            player.backend_name(),
                            feed_elapsed.as_secs_f32() * 1000.0,
                            frame.frame_index,
                            pts_ns as f64 / 1_000_000.0,
                        );
                    }
                    if wants_decode_latency && play_result.is_ok() {
                        client.report_decode_us(u32::try_from(feed_elapsed.as_micros()).unwrap_or(u32::MAX));
                    }
                    if let Err(e) = play_result {
                        tracing::warn!(
                            "{} error (frame {}, pts {:.2}ms): {e:#}",
                            player.backend_name(),
                            frame.frame_index,
                            pts_ns as f64 / 1_000_000.0,
                        );
                        if last_keyframe_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL) {
                            let _ = client.request_keyframe();
                            let _ = player.flush();
                            last_keyframe_request = Some(Instant::now());
                            holding = true;
                            hold_started.get_or_insert_with(Instant::now);
                        }
                    }
                }
            }
            Err(punktfunk_core::PunktfunkError::NoFrame) => {
                if last_heartbeat.elapsed() >= Duration::from_secs(2) {
                    last_heartbeat = Instant::now();
                    tracing::debug!("video: {frames_received} frames (idle)");
                }
            }
            Err(e) => {
                tracing::error!("video pump: {e:#}");
                break;
            }
        }

        if audio_offloaded {
            pump_ndl_audio(&client, &player);
        }

        if is_hdr {
            if let Ok(meta) = client.next_hdr_meta(Duration::ZERO) {
                if let Err(e) = player.set_color_info(Some(&meta), client.color) {
                    tracing::warn!("{} set_color_info: {e:#}", player.backend_name());
                }
            }
        }
    }
}

/// Drains raw Opus packets straight into NDL, for the offloaded path.
///
/// Unlike the software path this does NOT have to run on the main thread — the
/// main-thread constraint is `sdl2::audio::AudioQueue` being `!Send`, and there is no
/// `AudioQueue` here. It runs on the video pump thread, which also means audio keeps
/// flowing across a main-loop stall rather than hitching with it.
fn pump_ndl_audio(client: &NativeClient, player: &VideoPlayer) {
    while let Ok(packet) = client.next_audio(Duration::ZERO) {
        if let Err(e) = player.play_audio(&packet.data) {
            tracing::warn!("NDL audio error (seq {}): {e:#}", packet.seq);
        }
    }
}

/// Drains and plays all pending audio packets (non-blocking). Call once per main-loop
/// tick; runs on the main thread because `sdl2::audio::AudioQueue` is `!Send`.
pub fn pump_audio_once(client: &NativeClient, audio: &mut crate::audio::AudioPlayer) {
    use crate::audio::AudioEvent;
    // Logged roughly once/sec (200 packets @ 5ms/frame).
    static PACKET_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    while let Ok(packet) = client.next_audio(Duration::ZERO) {
        match audio.play(packet.seq, &packet.data) {
            Ok((peak, event)) => {
                match event {
                    // The two queue-too-full cases and the starved case are each audible
                    // and have different causes, so they are never collapsed into one
                    // message: `Underrun` means this thread was too slow, `Dropped`/
                    // `Resnapped` mean audio arrived faster than realtime.
                    AudioEvent::Underrun => {
                        tracing::debug!("audio underrun (device queue ran dry before this packet)");
                    }
                    AudioEvent::Resnapped => {
                        tracing::debug!(
                            "audio resnapped (queue was >{}ms behind)",
                            crate::audio::MAX_QUEUED_LAG_MS
                        );
                    }
                    AudioEvent::Dropped => {
                        tracing::debug!(
                            "audio packet dropped (queue >{}ms, draining)",
                            crate::audio::SOFT_QUEUED_LAG_MS
                        );
                    }
                    AudioEvent::Queued => {
                        let n = PACKET_COUNT.fetch_add(1, Ordering::Relaxed);
                        if n % 200 == 0 {
                            tracing::debug!("audio peak: {peak:.4}");
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("audio error (seq {}): {e:#}", packet.seq);
            }
        }
    }
}

/// Sends one input event to the host.
pub fn send_input(client: &NativeClient, ev: &InputEvent) -> Result<()> {
    client.send_input(ev).context("send_input")
}
