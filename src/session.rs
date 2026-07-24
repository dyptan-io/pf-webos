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

    fn set_color_info(&self, meta: Option<&quic::HdrMeta>, color: quic::ColorInfo) -> anyhow::Result<()> {
        match self {
            Self::Starfish(sf) => sf.set_color_info(meta, color),
            Self::Ndl(ndl) => ndl.set_color_info(meta, color),
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
        0, // client_caps: this client composites the host cursor into the video, not locally
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
                log,
            ) {
                Ok(sf) => {
                    writeln!(
                        log,
                        "Starfish loaded ({codec:?} {}x{}@{fps}fps, display {display_w}x{display_h})",
                        resolved_mode.width, resolved_mode.height,
                    )?;
                    VideoPlayer::Starfish(sf)
                }
                Err(e) => {
                    writeln!(log, "Starfish load failed ({e:#}) — falling back to NDL")?;
                    let ndl = NdlVideo::load(
                        &app_id,
                        resolved_mode.width as i32,
                        resolved_mode.height as i32,
                        codec,
                    )
                    .context("NDL load (Starfish fallback)")?;
                    writeln!(
                        log,
                        "NDL loaded ({codec:?} {}x{}@{fps}fps)",
                        resolved_mode.width, resolved_mode.height,
                    )?;
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
        writeln!(log, "{} colour metadata failed: {e:#}", player.backend_name())?;
    }
    writeln!(
        log,
        "colour metadata sent: hdr={is_hdr} transfer={} primaries={} matrix={} full_range={}",
        client.color.transfer, client.color.primaries, client.color.matrix, client.color.full_range,
    )?;

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StreamStats::default());
    let video_client = client.clone();
    let video_stop = stop.clone();
    let video_stats = stats.clone();
    let mut video_log = log.try_clone().context("clone log")?;
    let video_thread = std::thread::Builder::new()
        .name("punktfunk-webos-video".into())
        .spawn(move || video_pump(video_client, player, video_stop, video_stats, is_hdr, &mut video_log))
        .context("spawn video thread")?;

    Ok(Connected { client, stop, stats, video_thread })
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
pub fn request_access(
    host: &str,
    port: u16,
    identity: (String, String),
    timeout: Duration,
) -> Result<[u8; 32]> {
    let mode = Mode { width: 1280, height: 720, refresh_hz: 60 };
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
fn spawn_vendor_decode_thread_renicer(mut log: std::fs::File) {
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
                        let _ = writeln!(
                            log,
                            "setpriority(vendor thread {comm}, tid={tid}) failed: {}",
                            std::io::Error::last_os_error()
                        );
                    } else {
                        let _ = writeln!(log, "reniced vendor decode thread {comm} (tid={tid}) to -10");
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

fn video_pump(
    client: Arc<NativeClient>,
    player: VideoPlayer,
    stop: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
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
    match log.try_clone() {
        Ok(renicer_log) => spawn_vendor_decode_thread_renicer(renicer_log),
        Err(e) => {
            let _ = writeln!(log, "clone log for decode-thread renicer failed: {e:#}");
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
                stats.frames.store(frames_received, Ordering::Relaxed);
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
                    stats.holding.store(true, Ordering::Relaxed);
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
                    stats.holding.store(false, Ordering::Relaxed);
                    hold_started = None;

                    let pts_ns = frame.pts_ns;
                    let (play_result, feed_elapsed) = player.play(&frame.data, pts_ns);
                    stats
                        .feed_us
                        .store(u32::try_from(feed_elapsed.as_micros()).unwrap_or(u32::MAX), Ordering::Relaxed);

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
                if let Err(e) = player.set_color_info(Some(&meta), client.color) {
                    let _ = writeln!(log, "{} set_color_info: {e:#}", player.backend_name());
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
