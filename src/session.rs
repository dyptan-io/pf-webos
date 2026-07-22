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
use punktfunk_core::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR};
use punktfunk_core::quic;
use sdl2::controller::GameController;

use crate::ndl::{NdlCodec, NdlVideo};

pub struct Connected {
    pub client: Arc<NativeClient>,
    pub stop: Arc<AtomicBool>,
    /// Kept (not discarded) so [`Connected::shutdown`] can join it — otherwise this
    /// thread's `Arc<NativeClient>` clone could outlive process exit and `client`'s
    /// refcount would never hit zero, so `NativeClient::Drop`'s worker-join (which
    /// sends the real QUIC close frame) would never run.
    video_thread: std::thread::JoinHandle<()>,
}

impl Connected {
    /// Stops and joins the video thread, then drops the last `client` reference so
    /// `NativeClient::Drop` can run to completion instead of racing process exit.
    /// Call `self.client.disconnect_quit()` first for a deliberate stop (long-press-
    /// Back, app quit, SIGTERM) so the host tears the virtual display down
    /// immediately; skip it (e.g. "host ended the session") for a plain disconnect.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.video_thread.join();
        drop(self.client);
    }
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
        "connected: codec={} compositor={:?} audio_channels={} color={:?} resolved_bitrate_kbps={} \
         wants_decode_latency={} fingerprint={fp_hex}",
        client.codec,
        client.resolved_compositor,
        client.audio_channels,
        client.color,
        client.resolved_bitrate_kbps,
        client.wants_decode_latency()
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
    let video_thread = std::thread::Builder::new()
        .name("punktfunk-webos-video".into())
        .spawn(move || video_pump(video_client, ndl, video_stop, is_hdr, &mut video_log))
        .context("spawn video thread")?;

    Ok(Connected {
        client,
        stop,
        video_thread,
    })
}

/// Below this, one `request_keyframe` per unrecoverable-drop increase would flood the
/// control stream — decode stays wedged for several frames until the recovery lands
/// regardless, so throttle to roughly this cadence (matches the embedding guide's
/// "≤ ~1/100ms" guidance).
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Longest `video_pump` will withhold frames from NDL waiting for a clean re-anchor before
/// giving up and resuming anyway. Generous relative to a real recovery round-trip (RTT plus
/// encode plus decode, sub-second on any live link), but bounded so a request that's lost,
/// ignored, or answered without the reanchor flag we expect can never turn into a permanently
/// frozen picture. Worst case reverts to showing one concealed or corrupted frame, the
/// pre-freeze-until-reanchor behavior, instead of hanging forever.
const HOLD_GIVE_UP: Duration = Duration::from_secs(2);

/// Above this, `NDL_DirectVideoPlay` is applying backpressure rather than accepting the
/// access unit promptly — worth a log line even though (per aurora-tv's own investigation
/// of the same NDL backend, see `docs/NOTES.md`) there's no known client-side fix once it
/// happens; it's diagnostic signal for telling decoder-side stalls apart from network loss.
const NDL_FEED_BACKPRESSURE_WARN: Duration = Duration::from_millis(20);

fn video_pump(client: Arc<NativeClient>, ndl: NdlVideo, stop: Arc<AtomicBool>, is_hdr: bool, log: &mut std::fs::File) {
    // Register this thread alongside punktfunk-core's own internal data-plane pump thread
    // (UDP receive + FEC reassembly — already auto-registered) in the shared hot-thread
    // registry, then boost both with a best-effort `nice` priority bump. `hot_thread_ids` is
    // the same registry the Android client feeds to an ADPF hint session so the CPU governor
    // favors the whole video pipeline; Linux (this OS) has no automatic consumer for it, so we
    // are the consumer. No CAP_SYS_NICE here for real SCHED_FIFO priority, so this is the
    // closest available substitute — on a weak ARM SoC juggling UI/audio/network/decode
    // threads, an unprioritized decode thread getting occasionally preempted is exactly the
    // kind of thing that shows up as uneven frame pacing distinct from network loss.
    client.register_hot_thread();
    for tid in client.hot_thread_ids() {
        // SAFETY: `setpriority` takes a plain tid and priority value, no pointers involved.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -10) } != 0 {
            let _ = writeln!(
                log,
                "setpriority(tid={tid}, -10) failed (expected without CAP_SYS_NICE): {}",
                std::io::Error::last_os_error()
            );
        }
    }

    let mut last_keyframe_request: Option<Instant> = None;
    let mut last_dropped_seen = client.frames_dropped();
    // Constant for the session (Automatic bitrate, non-PyroWave) — check once, not per frame.
    let wants_decode_latency = client.wants_decode_latency();
    // Freeze-until-reanchor: under punktfunk's infinite-GOP stream, unrecoverable loss doesn't
    // fail decode — it produces reference-missing delta frames NDL *silently conceals* (the
    // "mpeg breakup" artifact), and it never self-heals without a clean re-anchor. Upstream
    // punktfunk-core ships a shared `reanchor::ReanchorGate` for this (decode every frame, but
    // only *present* the concealed ones — every other client has a decode/present split to do
    // that with), but NDL_DirectVideoPlay (webOS's DirectMedia API 2, the latest available on
    // this OS — there is no v3) couples decode and present into one opaque call, with no way to
    // decode without displaying. So instead of presenting: while `holding`, skip calling
    // `ndl.play` entirely — the punch-through plane just keeps showing whatever it last rendered
    // — and resume only once a frame arrives that's independently decodable against the picture
    // NDL already has: a real IDR (`FLAG_SOF`) or an LTR-RFI `USER_FLAG_RECOVERY_ANCHOR` (both
    // default-on loss recovery on modern NVENC/Vulkan-Video hosts). A host's intra-refresh
    // `USER_FLAG_RECOVERY_POINT` wave can't be consumed this way — that healing needs every
    // intervening frame actually decoded, which holding skips — so those marks are ignored here;
    // on hosts limited to that fallback, the `frames_dropped` keyframe backstop below eventually
    // forces a real IDR instead.
    let mut holding = false;
    // First-observed-loss timestamp for the current hold — NOT reset by a fresh gap/drop while
    // already holding, so a run of small hiccups can't keep pushing the give-up deadline out
    // forever. `None` whenever `holding` is `false`.
    let mut hold_started: Option<Instant> = None;
    // Diagnostic heartbeat — cheap proof, independent of every other counter below, that AUs are
    // actually arriving from the session at all (vs. e.g. the host never starting an encode under
    // Automatic bitrate, which would silently loop on `PunktfunkError::NoFrame` forever with no
    // other log line to show it).
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
                        "video heartbeat: {frames_received} AUs received, holding={holding}, frames_dropped={}",
                        client.frames_dropped()
                    );
                }
                // Loss recovery — the part the embedding guide calls out as the one to
                // get right under punktfunk's infinite-GOP stream (no periodic IDRs,
                // so unrecoverable loss otherwise never self-heals). Cheap+idempotent;
                // call for every frame. `note_frame_index` itself throttles the RFI
                // request it may fire and returns whether it saw a forward gap — the
                // embedding guide's cue to start freezing. `frames_dropped` is the
                // backstop for when even the recovery frame was lost, throttled here
                // the same way.
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
                        "loss detected (gap={gap} dropped={dropped}, frame {}) — freezing display",
                        frame.frame_index
                    );
                    // Drop whatever NDL still has buffered so a held frame — or the eventual
                    // re-anchor — doesn't sit head-of-line blocked behind stale pre-loss frames.
                    let _ = ndl.flush();
                }
                // Keep re-asking on the same throttle for as long as we're still frozen, not just
                // once on the initial loss edge — a single lost/delayed keyframe request (or one
                // that lands but for whatever reason doesn't come back reanchor-flagged) must never
                // freeze the picture permanently. Mirrors `punktfunk_core::reanchor::ReanchorGate`'s
                // own backstop, which keeps re-requesting for as long as `is_holding()`.
                if holding && last_keyframe_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL) {
                    if let Err(e) = client.request_keyframe() {
                        let _ = writeln!(log, "request_keyframe failed while holding: {e:#}");
                    }
                    last_keyframe_request = Some(Instant::now());
                }

                let is_reanchor =
                    frame.flags & u32::from(FLAG_SOF) != 0 || frame.flags & USER_FLAG_RECOVERY_ANCHOR != 0;
                // Never hold the picture forever: if this hold has run past `HOLD_GIVE_UP` with
                // no clean re-anchor observed (host never answered `request_keyframe`, the
                // answer never arrived reanchor-flagged, or NDL itself is the problem — any of
                // which turned into an indefinite freeze before this cap existed), resume feeding
                // NDL anyway. Worst case is back to the pre-freeze-until-reanchor behavior of a
                // moment of visible corruption; that beats a permanent frozen picture.
                let gave_up = hold_started.is_some_and(|t| t.elapsed() >= HOLD_GIVE_UP);
                if holding && !is_reanchor && !gave_up {
                    // Concealed/reference-broken frame — never feed it to NDL; the panel keeps
                    // showing the last good picture instead of the decoder's concealment output.
                } else {
                    if holding {
                        let _ = writeln!(
                            log,
                            "resuming feed after {:.0}ms holding (frame {}, flags=0x{:x}, reanchor={is_reanchor}, gave_up={gave_up})",
                            hold_started.map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0),
                            frame.frame_index,
                            frame.flags
                        );
                    }
                    holding = false;
                    hold_started = None;

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
                    // Feed the Automatic-bitrate controller's decode signal (see
                    // `punktfunk_core::abr` docs) — NDL couples decode+present into one opaque
                    // call, so `feed_elapsed` (this call's own duration, not counting any vsync
                    // wait since NDL_DirectVideoPlay doesn't block on presentation) is the closest
                    // proxy this backend can offer to the "decode-stage latency" the controller
                    // wants. Only for frames NDL actually accepted — a rejected AU never decoded.
                    if wants_decode_latency && play_result.is_ok() {
                        client.report_decode_us(u32::try_from(feed_elapsed.as_micros()).unwrap_or(u32::MAX));
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
                            holding = true;
                            hold_started.get_or_insert_with(Instant::now);
                        }
                    }
                }
            }
            Err(punktfunk_core::PunktfunkError::NoFrame) => {
                if last_heartbeat.elapsed() >= Duration::from_secs(2) {
                    last_heartbeat = Instant::now();
                    let _ = writeln!(
                        log,
                        "video heartbeat: {frames_received} AUs received (idle — none pending)"
                    );
                }
            }
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

/// Drains every pending rumble command from the shared policy engine (non-blocking,
/// like `pump_audio_once`) and applies it to the currently open controller. A single
/// pad is all this client tracks (see `gamepad.rs`), so `cmd.pad` is ignored rather
/// than matched against one; a command that arrives with no controller open is just
/// dropped.
pub fn pump_rumble_once(client: &NativeClient, controller: &mut Option<GameController>, log: &mut std::fs::File) {
    while let Ok(cmd) = client.next_rumble_command(Duration::ZERO) {
        if let Some(c) = controller {
            if let Err(e) = c.set_rumble(cmd.low, cmd.high, cmd.backstop_ms) {
                let _ = writeln!(log, "controller rumble failed: {e}");
            }
        }
    }
}

/// Sends one input event to the host; errors are logged by the caller (a send failure
/// here just means this one event was dropped — not fatal to the session).
pub fn send_input(client: &NativeClient, ev: &InputEvent) -> Result<()> {
    client.send_input(ev).context("send_input")
}
