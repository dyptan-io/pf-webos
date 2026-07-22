//! Connects to a punktfunk host and drives the video/audio hardware pipelines.
//!
//! Video runs on a dedicated thread ([`video_pump`]) behind a [`VideoPlayer`] abstraction
//! over the NDL and Starfish backends.
//!
//! Audio is drained from the main thread ([`pump_audio_once`]) because
//! `sdl2::audio::AudioQueue` is `!Send`.
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
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

    fn set_hdr_info(&self, meta: &quic::HdrMeta, color: quic::ColorInfo) -> anyhow::Result<()> {
        match self {
            Self::Starfish(sf) => sf.set_hdr_info(meta, color),
            Self::Ndl(ndl) => ndl.set_hdr_info(meta, color),
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
    /// Kept alive so [`Connected::shutdown`] can join it and ensure `NativeClient::Drop`
    /// (which sends the QUIC close frame) runs to completion before process exit.
    video_thread: std::thread::JoinHandle<()>,
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
    identity: (String, String),
    pin: Option<[u8; 32]>,
    launch: Option<String>,
    timeout: Duration,
    display_w: i32,
    display_h: i32,
    video_backend: VideoBackend,
    log: &mut std::fs::File,
) -> Result<Connected> {
    // VIDEO_CAP_CHACHA20: unconditional — armv7 has no hardware AES, so ChaCha20 is
    // faster. A ≥0.17.2 host picks it up; older hosts ignore the unknown bit.
    let video_caps = quic::VIDEO_CAP_CHACHA20
        | if hdr_enabled { quic::VIDEO_CAP_10BIT | quic::VIDEO_CAP_HDR } else { 0 };
    let display_hdr = hdr_enabled.then(cx_display_hdr);

    let client = NativeClient::connect(
        host,
        port,
        mode,
        CompositorPref::Auto,
        punktfunk_core::config::GamepadPref::Auto,
        bitrate_kbps,
        video_caps,
        2, // stereo
        quic::CODEC_HEVC | quic::CODEC_H264,
        0, // let the host choose
        display_hdr,
        launch,
        pin,
        Some(identity),
        timeout,
    )
    .context("connect")?;
    let client = Arc::new(client);

    let fp_hex = client
        .host_fingerprint
        .iter()
        .fold(String::new(), |mut s, b| { let _ = write!(s, "{b:02x}"); s });
    writeln!(
        log,
        "connected: codec={} compositor={:?} audio_ch={} color={:?} bitrate_kbps={} \
         decode_latency={} caps=0x{video_caps:02x} fp={fp_hex}",
        client.codec,
        client.resolved_compositor,
        client.audio_channels,
        client.color,
        client.resolved_bitrate_kbps,
        client.wants_decode_latency(),
    )?;

    let resolved_mode = client.mode();
    let fps = resolved_mode.refresh_hz.max(1);
    let codec = NdlCodec::from_wire(client.codec)
        .with_context(|| format!("unsupported codec 0x{:02x}", client.codec))?;
    let app_id = std::env::var("APPID").unwrap_or_else(|_| "io.dyptan.punktfunk.webos".into());

    let player = match video_backend {
        VideoBackend::Starfish => {
            let sf = StarfishVideo::load(
                &app_id,
                resolved_mode.width as i32,
                resolved_mode.height as i32,
                fps,
                codec,
                display_w,
                display_h,
                log,
            )
            .context("Starfish load")?;
            writeln!(
                log,
                "Starfish loaded ({codec:?} {}x{}@{fps}fps, display {display_w}x{display_h})",
                resolved_mode.width, resolved_mode.height,
            )?;
            VideoPlayer::Starfish(sf)
        }
        VideoBackend::Ndl => {
            let ndl = NdlVideo::load(
                &app_id,
                resolved_mode.width as i32,
                resolved_mode.height as i32,
                codec,
            )
            .context("NDL load")?;
            writeln!(
                log,
                "NDL loaded ({codec:?} {}x{}@{fps}fps)",
                resolved_mode.width, resolved_mode.height,
            )?;
            VideoPlayer::Ndl(ndl)
        }
    };

    let is_hdr = client.color.is_hdr();
    if is_hdr {
        if let Err(e) = player.set_hdr_info(&cx_display_hdr(), client.color) {
            writeln!(log, "{} initial HDR metadata failed: {e:#}", player.backend_name())?;
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let video_client = client.clone();
    let video_stop = stop.clone();
    let mut video_log = log.try_clone().context("clone log")?;
    let video_thread = std::thread::Builder::new()
        .name("punktfunk-webos-video".into())
        .spawn(move || video_pump(video_client, player, video_stop, is_hdr, &mut video_log))
        .context("spawn video thread")?;

    Ok(Connected { client, stop, video_thread })
}

/// Throttle for keyframe requests during hold or decode errors.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Freeze duration after which we resume even without a clean re-anchor.
const HOLD_GIVE_UP: Duration = Duration::from_secs(2);
/// Feed calls slower than this suggest decoder backpressure rather than network loss.
const FEED_BACKPRESSURE_WARN: Duration = Duration::from_millis(20);

fn video_pump(
    client: Arc<NativeClient>,
    player: VideoPlayer,
    stop: Arc<AtomicBool>,
    is_hdr: bool,
    log: &mut std::fs::File,
) {
    client.register_hot_thread();
    for tid in client.hot_thread_ids() {
        // SAFETY: plain syscall — tid and priority value only, no pointers.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -10) } != 0 {
            let _ = writeln!(
                log,
                "setpriority(tid={tid}) failed (expected without CAP_SYS_NICE): {}",
                std::io::Error::last_os_error()
            );
        }
    }

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
                if last_heartbeat.elapsed() >= Duration::from_secs(2) {
                    last_heartbeat = Instant::now();
                    let _ = writeln!(
                        log,
                        "video: {frames_received} frames, holding={holding}, dropped={}",
                        client.frames_dropped()
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
                    hold_started = Some(Instant::now());
                    let _ = writeln!(
                        log,
                        "loss (gap={gap} dropped={dropped}, frame {}) — freezing",
                        frame.frame_index
                    );
                    let _ = player.flush();
                }
                if holding
                    && last_keyframe_request
                        .is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL)
                {
                    if let Err(e) = client.request_keyframe() {
                        let _ = writeln!(log, "request_keyframe: {e:#}");
                    }
                    last_keyframe_request = Some(Instant::now());
                }

                let is_reanchor = frame.flags & u32::from(FLAG_SOF) != 0
                    || frame.flags & USER_FLAG_RECOVERY_ANCHOR != 0;
                let gave_up = hold_started.is_some_and(|t| t.elapsed() >= HOLD_GIVE_UP);
                if holding && !is_reanchor && !gave_up {
                    // Still frozen — drop this concealed frame, but fall through to the
                    // HDR poll below instead of `continue`ing past it.
                } else {
                    if holding {
                        let _ = writeln!(
                            log,
                            "resuming after {:.0}ms (frame {}, flags=0x{:x}, reanchor={is_reanchor}, gave_up={gave_up})",
                            hold_started.map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0),
                            frame.frame_index,
                            frame.flags,
                        );
                    }
                    holding = false;
                    hold_started = None;

                    let pts_ns = frame.pts_ns;
                    let (play_result, feed_elapsed) = player.play(&frame.data, pts_ns);

                    if feed_elapsed >= FEED_BACKPRESSURE_WARN {
                        let _ = writeln!(
                            log,
                            "{} slow: {:.1}ms (frame {}, pts {:.2}ms)",
                            player.backend_name(),
                            feed_elapsed.as_secs_f32() * 1000.0,
                            frame.frame_index,
                            pts_ns as f64 / 1_000_000.0,
                        );
                    }
                    if wants_decode_latency && play_result.is_ok() {
                        client.report_decode_us(
                            u32::try_from(feed_elapsed.as_micros()).unwrap_or(u32::MAX),
                        );
                    }
                    if let Err(e) = play_result {
                        let _ = writeln!(
                            log,
                            "{} error (frame {}, pts {:.2}ms): {e:#}",
                            player.backend_name(),
                            frame.frame_index,
                            pts_ns as f64 / 1_000_000.0,
                        );
                        if last_keyframe_request
                            .is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL)
                        {
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
                    let _ = writeln!(log, "video: {frames_received} frames (idle)");
                }
            }
            Err(e) => {
                let _ = writeln!(log, "video pump: {e:#}");
                break;
            }
        }

        if is_hdr {
            if let Ok(meta) = client.next_hdr_meta(Duration::ZERO) {
                if let Err(e) = player.set_hdr_info(&meta, client.color) {
                    let _ = writeln!(log, "{} set_hdr_info: {e:#}", player.backend_name());
                }
            }
        }
    }
}

/// Drains and plays all pending audio packets (non-blocking). Call once per main-loop
/// tick; runs on the main thread because `sdl2::audio::AudioQueue` is `!Send`.
pub fn pump_audio_once(
    client: &NativeClient,
    audio: &mut crate::audio::AudioPlayer,
    log: &mut std::fs::File,
) {
    // Logged roughly once/sec (200 packets @ 5ms/frame).
    static PACKET_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    while let Ok(packet) = client.next_audio(Duration::ZERO) {
        match audio.play(&packet.data) {
            Ok((peak, resnapped)) => {
                if resnapped {
                    let _ = writeln!(
                        log,
                        "audio resnapped (queue was >{}ms behind)",
                        crate::audio::MAX_QUEUED_LAG_MS
                    );
                }
                let n = PACKET_COUNT.fetch_add(1, Ordering::Relaxed);
                if n % 200 == 0 {
                    let _ = writeln!(log, "audio peak: {peak:.4}");
                }
            }
            Err(e) => {
                let _ = writeln!(log, "audio error (seq {}): {e:#}", packet.seq);
            }
        }
    }
}

/// Sends one input event to the host.
pub fn send_input(client: &NativeClient, ev: &InputEvent) -> Result<()> {
    client.send_input(ev).context("send_input")
}
