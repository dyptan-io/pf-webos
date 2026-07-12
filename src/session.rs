//! Connects to a punktfunk host and pumps video access units into NDL. Built directly
//! on `punktfunk_core::client::NativeClient` rather than `pf-client-core`'s
//! `session::start()` — that crate's `[target.'cfg(any(target_os = "linux", windows))']`
//! dependency table (FFmpeg/PipeWire/SDL3) activates on our target too (it also
//! reports `target_os = "linux"`), and none of those are available or needed here: we
//! decode video via NDL (hardware, OS-native) and audio via plain SDL2, not `FFmpeg`.
//! See the `punktfunk-webos` plan/memory notes for the full rationale.
//!
//! Audio is pumped from the *main thread's* event loop (`main.rs`), not a spawned
//! thread like video — `sdl2::audio::AudioQueue` wraps an `Rc`-based `AudioSubsystem`
//! internally (confirmed via the sdl2 crate source: `SubsystemDrop` is `Rc`-backed),
//! so it isn't `Send` and can't be moved into a new OS thread. `pump_audio_once`
//! below is the non-blocking drain call `main.rs`'s loop makes each tick.
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, Mode};
use punktfunk_core::input::InputEvent;
use punktfunk_core::quic;

use crate::ndl::{NdlCodec, NdlVideo};

pub struct Connected {
    pub client: Arc<NativeClient>,
    pub stop: Arc<AtomicBool>,
}

/// A reasonable static HDR10 mastering-metadata default for the CX's OLED panel —
/// BT.2020 primaries, D65 white point, ~800 nit peak (typical quoted CX highlight
/// brightness), near-zero OLED black level. Sent as `Hello::display_hdr` so the
/// host's virtual display EDID (and its tone-mapping) matches this panel; the
/// host's own per-content metadata (`next_hdr_meta`) is what actually gets forwarded
/// to NDL once connected — this is just what we advertise up front.
fn cx_display_hdr() -> quic::HdrMeta {
    quic::HdrMeta {
        // G, B, R order (ST.2086 convention) in 1/50000 chromaticity units — BT.2020.
        display_primaries: [[8_500, 39_850], [6_550, 2_300], [35_400, 14_600]],
        white_point: [15_635, 16_450], // D65
        max_display_mastering_luminance: 800 * 10_000,
        min_display_mastering_luminance: 5, // ~0.0005 cd/m², OLED near-black
        max_cll: 800,
        max_fall: 150,
    }
}

/// Connects and starts the video pump thread (NDL feed). Blocks until the handshake
/// completes or `timeout` elapses. `pin` = the pinned host fingerprint from a prior
/// pairing (`None` = trust-on-first-use — the caller should persist
/// `client.host_fingerprint` after a successful connect).
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
    // The physical panel size, for NDL's punch-through `set_area` — independent of
    // `mode` (the negotiated *stream* resolution): a 1080p stream on a 4K panel
    // must still punch through the full 4K rect (NDL upscales), not a top-left
    // quarter of it.
    display_w: i32,
    display_h: i32,
    log: &mut std::fs::File,
) -> Result<Connected> {
    let video_caps = if hdr_enabled {
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
        2, // audio_channels: stereo (webOS backend only wires stereo out today)
        quic::CODEC_HEVC | quic::CODEC_H264,
        0, // preferred_codec: 0 = let the host choose
        display_hdr,
        launch,
        pin,
        Some(identity),
        timeout,
    )
    .context("connect")?;
    let client = Arc::new(client);
    let fp_hex = client.host_fingerprint.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    });
    writeln!(
        log,
        "connected: codec={} compositor={:?} audio_channels={} color={:?} fingerprint={fp_hex}",
        client.codec, client.resolved_compositor, client.audio_channels, client.color
    )?;

    let resolved_mode = client.mode();
    let codec = NdlCodec::from_wire(client.codec)
        .with_context(|| format!("host resolved an unsupported codec bit {}", client.codec))?;
    let app_id = std::env::var("APPID").unwrap_or_else(|_| "io.dyptan.punktfunk.webos".into());
    let ndl =
        NdlVideo::load(&app_id, resolved_mode.width as i32, resolved_mode.height as i32, codec).context("NDL load")?;
    ndl.set_area(0, 0, display_w, display_h).context("NDL set_area")?;
    writeln!(
        log,
        "NDL video loaded ({codec:?} {}x{}, punch-through area {display_w}x{display_h})",
        resolved_mode.width, resolved_mode.height
    )?;

    // `color.is_hdr()` (PQ or HLG transfer) only resolves true when the host actually
    // negotiated an HDR encode (our video_caps request above).
    let is_hdr = client.color.is_hdr();

    // NDL never picks up the TV's HDR picture mode from the bitstream itself — only
    // an explicit `NDL_DirectVideoSetHDRInfo` call flips it, and it needs to land
    // before/at the first frames NDL renders, not whenever the host's per-content
    // HdrMeta datagram (best-effort, "one near session start") happens to arrive.
    // Waiting on that alone left the TV in SDR picture mode indefinitely whenever
    // that one datagram was lost. So set a reasonable default immediately here —
    // same mastering values already advertised in `Hello::display_hdr` — and let
    // `video_pump`'s continuous poll below refine it once/if the host's real
    // per-content metadata shows up.
    if is_hdr {
        if let Err(e) = ndl.set_hdr_info(&cx_display_hdr(), client.color) {
            writeln!(log, "NDL set_hdr_info (initial) failed: {e:#}")?;
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let video_client = client.clone();
    let video_stop = stop.clone();
    let mut video_log = log.try_clone().context("clone log for video thread")?;
    std::thread::Builder::new()
        .name("punktfunk-webos-video".into())
        .spawn(move || video_pump(video_client, ndl, video_stop, is_hdr, &mut video_log))
        .context("spawn video thread")?;

    Ok(Connected { client, stop })
}

/// Below this, one `request_keyframe` per unrecoverable-drop increase would flood the
/// control stream — decode stays wedged for several frames until the recovery lands
/// regardless, so throttle to roughly this cadence (matches the embedding guide's
/// "≤ ~1/100ms" guidance).
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Above this, `NDL_DirectVideoPlay` is applying backpressure rather than accepting the
/// access unit promptly — worth a log line even though (per aurora-tv's own investigation
/// of the same NDL backend, see `docs/NOTES.md`) there's no known client-side fix once it
/// happens; it's diagnostic signal for telling decoder-side stalls apart from network loss.
const NDL_FEED_BACKPRESSURE_WARN: Duration = Duration::from_millis(20);

fn video_pump(client: Arc<NativeClient>, ndl: NdlVideo, stop: Arc<AtomicBool>, is_hdr: bool, log: &mut std::fs::File) {
    let mut last_keyframe_request: Option<Instant> = None;
    let mut last_dropped_seen = client.frames_dropped();

    while !stop.load(Ordering::Relaxed) {
        match client.next_frame(Duration::from_millis(500)) {
            Ok(frame) => {
                // Loss recovery — the part the embedding guide calls out as the one to
                // get right under punktfunk's infinite-GOP stream (no periodic IDRs,
                // so unrecoverable loss otherwise never self-heals). Cheap+idempotent;
                // call for every frame. `note_frame_index` itself throttles the RFI
                // request it may fire; `frames_dropped` is the backstop for when even
                // the recovery frame was lost, throttled here the same way.
                client.note_frame_index(frame.frame_index);
                let dropped_now = client.frames_dropped();
                if dropped_now > last_dropped_seen {
                    last_dropped_seen = dropped_now;
                    if last_keyframe_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL) {
                        let _ = client.request_keyframe();
                        // Drop whatever NDL still has buffered so the recovery
                        // keyframe doesn't sit head-of-line blocked behind stale
                        // pre-loss frames once it arrives.
                        let _ = ndl.flush();
                        last_keyframe_request = Some(Instant::now());
                    }
                }

                let feed_start = Instant::now();
                let play_result = ndl.play(&frame.data);
                let feed_elapsed = feed_start.elapsed();
                if feed_elapsed >= NDL_FEED_BACKPRESSURE_WARN {
                    let _ = writeln!(
                        log,
                        "NDL_DirectVideoPlay slow: {:.1}ms (frame {}) — decoder backpressure, not network loss",
                        feed_elapsed.as_secs_f32() * 1000.0,
                        frame.frame_index
                    );
                }
                if let Err(e) = play_result {
                    let _ = writeln!(log, "NDL play error (frame {}): {e:#}", frame.frame_index);
                    // NDL rejected this access unit outright (e.g. a transient decoder hiccup
                    // around an HDR/mode change) — request a fresh keyframe so later P-frames
                    // don't decode against a reference NDL never actually accepted. Same throttle
                    // as the network-loss path above; matches the recovery aurora-tv added for
                    // its ss4s frontend's NOT_READY feed results left otherwise uncorrected.
                    if last_keyframe_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL) {
                        let _ = client.request_keyframe();
                        let _ = ndl.flush();
                        last_keyframe_request = Some(Instant::now());
                    }
                }
            }
            Err(punktfunk_core::PunktfunkError::NoFrame) => {}
            Err(e) => {
                let _ = writeln!(log, "video pump ending: {e:#}");
                break;
            }
        }

        // Content HDR mastering metadata can change over the life of a session (the
        // host doesn't just send it once) — a cheap non-blocking drain, applying the
        // latest to NDL, matches the embedding guide's "apply the latest" guidance.
        if is_hdr {
            if let Ok(meta) = client.next_hdr_meta(Duration::ZERO) {
                if let Err(e) = ndl.set_hdr_info(&meta, client.color) {
                    let _ = writeln!(log, "NDL set_hdr_info failed: {e:#}");
                }
            }
        }
    }
}

/// Drains and plays every audio packet currently queued (non-blocking) — call once
/// per main-loop tick. See module docs for why this runs on the main thread instead
/// of a spawned one like `video_pump`.
pub fn pump_audio_once(client: &NativeClient, audio: &mut crate::audio::AudioPlayer, log: &mut std::fs::File) {
    // Peak-amplitude sampling, logged roughly once/sec (200 packets @ 5ms/frame) —
    // tells "our own decode is silent" apart from "PulseAudio/TV output isn't
    // reaching the speaker" (PulseAudio-side inspection showed the stream reaching
    // a real, unmuted, 100%-volume hardware sink, so this checks the other end).
    static PACKET_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    while let Ok(packet) = client.next_audio(Duration::ZERO) {
        match audio.play(&packet.data) {
            Ok(peak) => {
                let n = PACKET_COUNT.fetch_add(1, Ordering::Relaxed);
                if n % 200 == 0 {
                    let _ = writeln!(log, "audio decode peak amplitude: {peak:.4}");
                }
            }
            Err(e) => {
                let _ = writeln!(log, "audio play error (seq {}): {e:#}", packet.seq);
            }
        }
    }
}

/// Sends one input event to the host; errors are logged by the caller (a send failure
/// here just means this one event was dropped — not fatal to the session).
pub fn send_input(client: &NativeClient, ev: &InputEvent) -> Result<()> {
    client.send_input(ev).context("send_input")
}
