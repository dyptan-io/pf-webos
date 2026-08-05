//! `GameStream` streaming: `/launch` plus the crate's blocking stream driver, wired to this
//! client's own video sink and audio device.
//!
//! **No hand-rolled transport.** `moonlight_common::stream::std::MoonlightStream` already drives
//! the sans-IO core — RTSP handshake, the three UDP drivers, the `ENet` control peer, all ping
//! obligations — calling back into [`VideoDecoder`]/[`AudioDecoder`]/[`ConnectionListener`] on its
//! own threads. So this module is just those three callbacks plus the settings mapping.
//!
//! Thread shape:
//! * **video** — the crate's video thread calls [`SinkDecoder`], which owns [`NdlSink`] exactly as
//!   punktfunk's `video_pump` does; NDL loads in `setup` on that thread because the pacer queries
//!   panel refresh through SDL on construction.
//! * **audio** — `sdl2::audio::AudioQueue` is `!Send` and lives on the main thread, so Opus frames
//!   cross a bounded channel, drained by [`GsStream::pump_audio_once`] each tick.
//! * **control** — HDR changes and lightbar colours arrive on the control thread. HDR goes through
//!   [`Shared::apply_hdr`], which the video thread also calls once NDL is loaded; the lightbar is
//!   left in an atomic for the main loop's tick, since only the newest value matters.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::stream::audio::{AudioConfig, AudioDecoder, AudioFrame, OpusMultistreamConfig};
use moonlight_common::stream::connection::ConnectionListener;
use moonlight_common::stream::control::ActiveGamepads;
use moonlight_common::stream::proto::control::input_batcher::ClientInputEvent;
use moonlight_common::stream::std::{MoonlightStream, MoonlightStreamError};
use moonlight_common::stream::video::{
    ColorRange, ColorSpace, DecodeResult, FrameType, SunshineHdrMetadata, VideoDecodeUnit, VideoDecoder, VideoFormats,
    VideoSetup,
};
use moonlight_common::stream::{AesIv, AesKey, EncryptionFlags, MoonlightStreamSettings, StreamingConfig};
use moonlight_common::AppId;
use punktfunk_core::input::InputEvent;
use punktfunk_core::quic;

use crate::backend::gamestream::input::InputTranslator;
use crate::backend::gamestream::query;
use crate::platform::webos::ndl::{NdlCodec, NdlVideo};
use crate::services::store::{CodecPref, ColorRangeOverride, GamepadType};
use crate::session::sink::{FrameFlags, NdlSink, SinkConfig, SinkResult, VideoPlayer};
use crate::session::{StreamStats, SHUTDOWN_JOIN_TIMEOUT};

/// Bytes per video packet on the wire. Moonlight's own default, and the value Sunshine is tuned
/// for; it is not a user setting on any client.
const PACKET_SIZE: u32 = 1392;

/// IDR requests share the `ENet` control channel with gamepad input, so an unthrottled request
/// loop directly inflates input latency — the whole subject of aurora-tv's
/// `idr-throttle-input-latency` patch. Ten times punktfunk's interval, which pays nothing for a
/// request on its own QUIC stream (see `session`'s own constant).
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Opus frames buffered between the crate's audio thread and the main loop's drain. At
/// `GameStream`'s 20 ms framing that is 640 ms of audio — far more than the ~2 ms tick needs, so
/// the bound is only reached if the main thread stalls, and then dropping the oldest frames is
/// what the software path does anyway (see `audio::SOFT_QUEUED_LAG_MS`).
const AUDIO_QUEUE_FRAMES: usize = 32;

/// Delay between connect attempts while waiting out `budget::HOST_WAIT`. Long enough that a booting
/// host isn't hammered, short enough that the stream starts promptly once it is ready.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Everything [`connect`] needs. A struct rather than the 15 positional arguments
/// `session::connect` grew — the plan wants that collapsed there too.
pub struct GsConnectSpec {
    pub addr: String,
    /// The host's plain-HTTP query port (the mDNS SRV port for a discovered host).
    pub query_port: u16,
    /// Decimal `<ID>` from `/applist`, or `None` for "whatever the host calls its desktop" —
    /// which is `Desktop` in the list, resolved here so no screen knows the format.
    pub app_id: Option<String>,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// 0 = Automatic, resolved client-side: `GameStream` has no host-side ABR to hand it to.
    pub bitrate_kbps: u32,
    pub hdr_enabled: bool,
    pub codec: CodecPref,
    pub color_range_override: ColorRangeOverride,
    pub video_pacing: bool,
    pub gamepad_type: GamepadType,
    /// Whether a controller is attached, so the host builds a virtual pad at launch instead of
    /// waiting for the first `ControllerConnect`.
    pub gamepad_attached: bool,
    /// How long to keep re-trying a host that answers but isn't ready to stream. Decided at the
    /// runtime seam, where punktfunk's equivalent budget is also chosen — see `budget::HOST_WAIT`.
    pub host_wait: Duration,
}

/// A live `GameStream` session.
pub struct GsStream {
    stream: Arc<MoonlightStream>,
    /// Cloned by the runtime for the HID-mouse reader thread, which sends off the main loop.
    input: Arc<GsInput>,
    pub stats: Arc<StreamStats>,
    shared: Arc<Shared>,
    audio_rx: mpsc::Receiver<Vec<u8>>,
    /// Whether HDR is being applied this session; drives the runtime's Game-mode pick.
    pub hdr: bool,
    /// The negotiated codec, for the stats overlay's header.
    pub codec: NdlCodec,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// What `/launch` asked the host to encode at, for the stats overlay: there is no host-side
    /// ABR to report a live figure (`BackendCaps::host_abr` is false).
    pub bitrate_kbps: u32,
}

/// The session's input path, shared: the main loop sends from its tick, the HID-mouse reader from
/// its own thread (see `runtime::stream`'s `hid_mouse`).
///
/// One mutex around the translator, because `GameStream`'s gamepad packets are whole-pad state —
/// two threads folding into that state unsynchronized would ship a mix of both. The lock is held
/// only for the translation itself, and mouse motion at 1000 Hz is the same order of contention as
/// the batcher's own lock inside `send_input`.
pub struct GsInput {
    stream: Arc<MoonlightStream>,
    state: Mutex<InputState>,
}

struct InputState {
    translator: InputTranslator,
    /// Reused buffer — see [`InputTranslator::translate`].
    out: Vec<ClientInputEvent>,
}

impl GsInput {
    /// Translates and batches one event. Errors are logged, not returned: a control-stream failure
    /// ends the session through [`GsStream::is_session_ended`], and a caller that had to handle
    /// every keystroke's error would have nothing better to do with it.
    pub fn send(&self, ev: &InputEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let InputState { translator, out } = &mut *state;
        out.clear();
        translator.translate(ev, out);
        for gs in out.drain(..) {
            if let Err(e) = self.stream.send_input(gs) {
                tracing::warn!("GameStream send_input: {e}");
            }
        }
    }
}

/// The HDR state a session can be in: whether the panel should be in HDR, and the host's mastering
/// metadata if it sent any.
type HdrState = (bool, Option<SunshineHdrMetadata>);

/// The decoder handle and the panel state that goes with it — one lock, because every operation
/// here reads or writes both. Three separate mutexes had to be taken in a fixed order to apply one
/// HDR change, and "wanted" could move between reading it and comparing it with "applied".
struct Panel {
    /// `Some` once the video callback has loaded NDL, `None` again after [`GsStream::shutdown`]
    /// releases it — which is why this is not a `OnceLock`: dropping our handle at a known point is
    /// what keeps a later `ndl::quit()` from racing a live one.
    ndl: Option<Arc<NdlVideo>>,
    /// What the session should be in. Seeded from what `/launch` negotiated, because a host need not
    /// send an HDR control packet at all — waiting for one left the panel in SDR for the whole
    /// stream. The control thread overwrites it if the host does send one.
    wanted: HdrState,
    /// What was last pushed to NDL successfully. Re-applying `NDL_DirectVideoSetHDRInfo` on every
    /// host HDR packet re-enters panel HDR mode and drops a 120 Hz panel to 60 (see `docs/NOTES.md`
    /// and the CX finding), so this dedupes — but only records a state that actually reached the
    /// panel, so one that arrived before the NDL load isn't mistaken for one already applied.
    applied: Option<HdrState>,
}

/// State the three callback threads and the main loop all touch.
struct Shared {
    panel: Mutex<Panel>,
    /// The host ended the session (`ServerTermination`, or the control stream disconnecting).
    terminated: AtomicBool,
    /// `ServerTermination`'s reason code, 0 when the host said the user closed the app.
    termination_code: AtomicI32,
    /// The video callback has run its `stop` and released its NDL handle — see
    /// [`GsStream::shutdown`].
    video_stopped: AtomicBool,
    /// Last lightbar colour the host asked for, packed by [`pack_led`]: coalesced, because only the
    /// newest matters, and an atomic rather than a `Mutex<Option<…>>` so neither the control thread
    /// nor the main loop's tick can wait on the other for three bytes.
    led: AtomicU32,
    color_range_override: ColorRangeOverride,
}

/// `(r, g, b)` in the low three bytes with a presence bit above them; `0` is "nothing asked for".
/// A tuple in an atomic needs *some* encoding, and this one keeps `None` as the zero value, so the
/// initial state and the drained state are the same thing.
fn pack_led(r: u8, g: u8, b: u8) -> u32 {
    1 << 24 | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)
}

fn unpack_led(packed: u32) -> Option<(u8, u8, u8)> {
    (packed != 0).then_some(((packed >> 16) as u8, (packed >> 8) as u8, packed as u8))
}

impl Shared {
    /// Pushes the wanted HDR state to NDL, if it isn't already there.
    ///
    /// Called from two places, and needs both: the video thread once NDL is loaded (the host may
    /// never send an HDR packet, and a packet that arrived before the load had nothing to apply to)
    /// and the control thread when the host changes it mid-stream.
    ///
    /// A no-op while NDL is unloaded, and deliberately records nothing then — `SinkDecoder::setup`
    /// re-runs it after the load.
    fn apply_hdr(&self) {
        let Ok(mut panel) = self.panel.lock() else { return };
        if panel.applied == Some(panel.wanted) {
            return;
        }
        let (enabled, sunshine) = panel.wanted;
        let Some(ndl) = panel.ndl.clone() else {
            // Before the video thread's NDL load. Left unrecorded on purpose.
            tracing::debug!("GameStream HDR enabled={enabled} deferred: NDL not loaded yet");
            return;
        };
        // Metadata is not optional for HDR: `NdlVideo::set_color_info` sends *nothing* without it
        // (`NdlHdrInfo` has no "HDR without mastering data" form), so a host that enables HDR
        // without metadata — GFE, and Sunshine builds that omit it — would leave the panel in SDR.
        // The panel's own defaults stand in, exactly as punktfunk's `Hello::display_hdr` does.
        let meta = enabled.then(|| sunshine.map_or_else(crate::session::cx_display_hdr, hdr_meta));
        let mut color = if enabled {
            quic::ColorInfo::HDR10_BT2020_PQ
        } else {
            quic::ColorInfo::SDR_BT709
        };
        self.color_range_override.apply(&mut color);
        match ndl.set_color_info(meta.as_ref(), color) {
            Ok(()) => {
                tracing::info!(
                    "GameStream HDR applied: enabled={enabled} host_metadata={}",
                    sunshine.is_some()
                );
                panel.applied = Some(panel.wanted);
            }
            // Unrecorded, so the next change (or the next `setup`) tries again.
            Err(e) => tracing::warn!("NDL set_color_info: {e:#}"),
        }
    }
}

impl GsStream {
    /// The shared input path — clone it for a thread that sends off the main loop.
    pub fn input(&self) -> Arc<GsInput> {
        self.input.clone()
    }

    /// One input event from the main loop.
    pub fn send_input(&self, ev: &InputEvent) {
        self.input.send(ev);
    }

    /// Drains queued Opus frames into the SDL audio device. Call once per main-loop tick, exactly
    /// like `session::pump_audio_once` — the queue is fed from the crate's audio thread.
    pub fn pump_audio_once(&self, audio: &mut crate::platform::webos::audio::AudioPlayer) {
        // The device's own lag policy handles a backlog; `seq` is a plain counter because
        // `GameStream`'s audio stream already conceals its own loss (FEC + the crate's
        // depayloader), so there is no gap for `AudioGapTracker` to find.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        while let Ok(frame) = self.audio_rx.try_recv() {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = audio.play(seq, &frame) {
                tracing::warn!("GameStream audio error (seq {seq}): {e:#}");
            }
        }
    }

    /// Applies whatever pad feedback the host has asked for since the last tick. Call it where
    /// `session::pump_feedback_once` is called: the effects land on devices the main thread owns.
    ///
    /// **Lightbar only — an upstream limit, not a choice.** `moonlight-common`'s stream driver
    /// dispatches LED/motion/HDR/termination to `ConnectionListener` and drops every other
    /// server-bound control packet (`stream/std/mod.rs`, `TODO: implement other packets`), so
    /// rumble packets never reach us. Fixing that needs those arms added to the crate.
    pub fn pump_feedback_once(&self, feedback: Option<&mut crate::platform::webos::dualsense::Feedback>) {
        let Some(fb) = feedback else { return };
        let Some((r, g, b)) = unpack_led(self.shared.led.swap(0, Ordering::Relaxed)) else {
            return;
        };
        // `pad: 0` — the host's controller number is per-session and this client drives one
        // physical pad's lightbar; `Feedback` ignores the field anyway.
        fb.apply(&punktfunk_core::quic::HidOutput::Led { pad: 0, r, g, b });
    }

    /// Channels to open the audio device with. Always stereo: the launch request asks for it and
    /// `ChannelAudioDecoder::setup` fails the stream if the host negotiates anything else, so
    /// there is no other value this can ever report.
    pub const AUDIO_CHANNELS: u8 = 2;

    /// The host ended the session, or the transport dropped.
    pub fn is_session_ended(&self) -> bool {
        self.shared.terminated.load(Ordering::Relaxed) || self.stream.is_stopped()
    }

    /// The sentence to show when the session ended by itself, from the host's own reason code.
    /// Unlike punktfunk — where nothing distinguishes a graceful close from a network drop —
    /// `GameStream` sends a `ServerTermination` reason, so a DRM stop or a host encoder failure
    /// can say what it was instead of "the host closed the connection".
    pub fn end_message(&self) -> String {
        crate::errors::gamestream_end_message(self.shared.termination_code.load(Ordering::Relaxed))
    }

    /// Ask the host to end the session and stop the driver threads. Named to match
    /// `NativeClient::disconnect_quit` so the streaming loop reads the same for both protocols.
    pub fn disconnect_quit(&self) {
        self.stream.stop();
    }

    /// Stops the stream and releases NDL's handle. Returns whether teardown was clean, matching
    /// `session::Connected::shutdown`: `false` means the caller must skip `ndl::quit()`, because a
    /// thread that may still be inside an NDL call would race a concurrent unload.
    pub fn shutdown(self) -> bool {
        self.stream.stop();
        // Wait for the crate's video callback to have run its `stop`, which is what drops the
        // sink and with it one of the two `Arc<NdlVideo>` owners. Bounded for the reason
        // `SHUTDOWN_JOIN_TIMEOUT` documents: the FFI calls between the driver's own stop checks
        // have no timeout of their own, and a wedged vendor call must not freeze the app.
        let deadline = std::time::Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        while !self.shared.video_stopped.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let clean = self.shared.video_stopped.load(Ordering::Relaxed);
        if !clean {
            tracing::error!("GameStream video callback did not stop within {SHUTDOWN_JOIN_TIMEOUT:?} — leaking it");
        }
        // Our own handle goes last: while any `Arc<NdlVideo>` lives, the decoder stays loaded, so
        // releasing it only here is what keeps `play` from racing the unload.
        if let Ok(mut panel) = self.shared.panel.lock() {
            drop(panel.ndl.take());
        }
        clean
    }
}

/// Connects, launches (or resumes) the app, and starts the stream. Blocking — call it on the
/// connect thread, like `session::connect`.
pub fn connect(spec: GsConnectSpec) -> Result<GsStream> {
    let host = query::open(&spec.addr, Some(spec.query_port))?;
    if !host.is_paired().unwrap_or(false) {
        anyhow::bail!("this host hasn't been paired yet");
    }

    let app_id = resolve_app_id(&host, spec.app_id.as_deref())?;
    let want_hdr = spec.hdr_enabled && spec.codec != CodecPref::H264;
    let mut settings = MoonlightStreamSettings {
        width: spec.width,
        height: spec.height,
        fps: spec.refresh_hz,
        fps_x100: spec.refresh_hz * 100,
        bitrate: match spec.bitrate_kbps {
            0 => default_bitrate_kbps(spec.width, spec.height, spec.refresh_hz),
            explicit => explicit,
        },
        packet_size: PACKET_SIZE,
        // No stream encryption. It is optional on the video/audio planes (the control stream
        // encrypts regardless), and this armv7 SoC has no hardware AES — the same reason
        // punktfunk sessions ask for ChaCha20. Re-visit only with a measurement.
        encryption_flags: EncryptionFlags::NONE,
        // A TV on the same LAN as the host. `Auto` lets the host decide from the address, which
        // for a link-local address is the same answer with an extra guess in it.
        streaming_remotely: StreamingConfig::Local,
        // "Optimize game settings": let the host set the game's resolution to the stream's.
        sops: true,
        hdr: want_hdr,
        supported_video_formats: supported_formats(spec.codec, want_hdr),
        color_space: if want_hdr {
            ColorSpace::Rec2020
        } else {
            ColorSpace::Rec709
        },
        color_range: match spec.color_range_override {
            // HDR is always limited range; the override only speaks for SDR.
            _ if want_hdr => ColorRange::Limited,
            ColorRangeOverride::Full => ColorRange::Full,
            ColorRangeOverride::Limited | ColorRangeOverride::Auto => ColorRange::Limited,
        },
        // The TV plays the audio; the host must not also play it locally.
        local_audio_play_mode: false,
        // Stereo only for now: the software decoder's layout comes from punktfunk's
        // `layout_for`, which is 5 ms framing, and `GameStream`'s surround configs need the
        // multistream mapping from `OpusMultistreamConfig::from_surround_param`. Requesting more
        // than can be decoded would be silence, not surround.
        audio_config: AudioConfig::STEREO,
        gamepads_attached: if spec.gamepad_attached {
            ActiveGamepads::GAMEPAD_1
        } else {
            ActiveGamepads::empty()
        },
        gamepads_persist_after_disconnect: false,
        enable_mic: false,
    };
    // Older hosts don't take every setting; this is where an impossible request (4K on a host
    // that can't, HDR on a host without Main10) fails with a reason worth showing.
    settings
        .adjust_for_server(
            host.version().map_err(gs_err)?,
            &host.gfe_version().map_err(gs_err)?,
            host.server_codec_mode_support().map_err(gs_err)?,
        )
        .context("stream settings unsupported by this host")?;
    // What the host agreed to, which is not always what was asked: `adjust_for_server` clears HDR
    // on a host without Main10. Everything downstream (the panel push, the Game-mode pick, the
    // stats overlay) has to follow the negotiation, or an SDR stream gets an HDR infoframe.
    let hdr = settings.hdr;
    if hdr != want_hdr {
        tracing::info!("GameStream host adjusted HDR to {hdr}");
    }

    let bitrate_kbps = settings.bitrate;
    // A host answers `/serverinfo` well before it can start a session — see `budget::HOST_WAIT`.
    let deadline = std::time::Instant::now() + spec.host_wait;
    let started = loop {
        match launch_and_connect(&host, app_id, &settings, hdr, &spec) {
            Ok(started) => break started,
            Err(Failure::Final(e)) => return Err(e),
            Err(Failure::Starting(e)) => {
                // No room for another attempt: report the last failure rather than sleep out the
                // rest of the budget on a try that can't finish inside it.
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left < RETRY_INTERVAL {
                    return Err(e);
                }
                tracing::warn!(
                    "GameStream connect failed ({e:#}) — retrying, {}s of budget left",
                    left.as_secs()
                );
                std::thread::sleep(RETRY_INTERVAL);
            }
        }
    };

    Ok(GsStream {
        input: Arc::new(GsInput {
            stream: started.stream.clone(),
            state: Mutex::new(InputState {
                translator: InputTranslator::new(spec.gamepad_type),
                out: Vec::new(),
            }),
        }),
        stream: started.stream,
        stats: started.stats,
        shared: started.shared,
        audio_rx: started.audio_rx,
        hdr,
        codec: if spec.codec == CodecPref::H264 {
            NdlCodec::H264
        } else {
            NdlCodec::H265
        },
        width: spec.width,
        height: spec.height,
        refresh_hz: spec.refresh_hz,
        bitrate_kbps,
    })
}

/// One [`launch_and_connect`] failure, split by whether waiting could fix it.
enum Failure {
    /// What a host that is still starting produces: nothing was served, or the handshake never
    /// completed.
    Starting(anyhow::Error),
    /// The host answered with a reason — an unsupported setting, a dropped pairing. Retrying it
    /// would only park the user on a black scrim for the whole budget.
    Final(anyhow::Error),
}

/// A started session's parts, before [`GsStream`] wraps them.
struct Started {
    stream: Arc<MoonlightStream>,
    stats: Arc<StreamStats>,
    shared: Arc<Shared>,
    audio_rx: mpsc::Receiver<Vec<u8>>,
}

/// Everything the retry loop in [`connect`] repeats: `/launch` (or `/resume`) and the stream setup.
/// The per-attempt state is built here because the crate takes the decoders by value — a failed
/// attempt's are gone, and the next one needs its own.
fn launch_and_connect(
    host: &query::Host,
    app_id: AppId,
    settings: &MoonlightStreamSettings,
    hdr: bool,
    spec: &GsConnectSpec,
) -> Result<Started, Failure> {
    let crypto = RustCryptoBackend;
    let key = |what: &str, e: &dyn std::fmt::Display| Failure::Final(anyhow::anyhow!("{what}: {e}"));
    let aes_key = AesKey::new_random(&crypto).map_err(|e| key("aes key", &e))?;
    let aes_iv = AesIv::new_random(&crypto).map_err(|e| key("aes iv", &e))?;
    let config = host
        .start_stream(
            app_id,
            settings,
            aes_key,
            aes_iv,
            MoonlightStream::launch_query_parameters(),
        )
        .map_err(|e| {
            let starting = launch_unanswered(&e);
            let e = gs_err(e);
            if starting {
                Failure::Starting(e)
            } else {
                Failure::Final(e)
            }
        })?;
    tracing::info!(
        "GameStream launch ok: {}x{}@{} hdr={hdr} bitrate={} kbps app={} rtsp={:?}",
        settings.width,
        settings.height,
        settings.fps,
        settings.bitrate,
        app_id.0,
        config.rtsp_session_url,
    );

    let stats = Arc::new(StreamStats::default());
    stats.pacing_enabled.store(spec.video_pacing, Ordering::Relaxed);
    let shared = Arc::new(Shared {
        // `wanted` is what `/launch` negotiated; `set_hdr_mode` refines it if the host sends one.
        panel: Mutex::new(Panel {
            ndl: None,
            wanted: (hdr, None),
            applied: None,
        }),
        terminated: AtomicBool::new(false),
        termination_code: AtomicI32::new(0),
        video_stopped: AtomicBool::new(false),
        led: AtomicU32::new(0),
        color_range_override: spec.color_range_override,
    });
    let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_QUEUE_FRAMES);

    let video = SinkDecoder {
        sink: None,
        stats: stats.clone(),
        shared: shared.clone(),
        formats: settings.supported_video_formats,
        frames: 0,
        last_frame_index: None,
        au: Vec::new(),
    };
    let audio = ChannelAudioDecoder {
        tx: audio_tx,
        dropped: 0,
    };
    let listener = Listener { shared: shared.clone() };

    let stream = MoonlightStream::connect(config, settings.clone(), video, audio, listener, Arc::new(crypto) as _)
        .map_err(|e| {
            let error = anyhow::anyhow!("start GameStream stream: {e}");
            if setup_unanswered(&e) && shared.panel.lock().is_ok_and(|p| p.ndl.is_none()) {
                Failure::Starting(error)
            } else {
                Failure::Final(error)
            }
        })?;
    tracing::info!("GameStream host features: {:?}", stream.host_features());
    Ok(Started {
        stream: Arc::new(stream),
        stats,
        shared,
        audio_rx,
    })
}

/// Whether a failed `/launch` (or `/resume`) is worth re-sending: only when the host took the
/// request and never finished answering, which is what a host still bringing up the display and the
/// app looks like. A host that replied — "Invalid PIN", an unsupported mode — replied for good.
///
/// A refused connection is *not* retryable, even though it also means nothing was served: reaching
/// here at all means `/serverinfo` just answered, so the host serving `/launch` has gone away rather
/// than not arrived yet. Retrying that parked the user on a black scrim for the whole
/// `budget::HOST_WAIT` where a punktfunk host reports it after one handshake.
fn launch_unanswered(err: &moonlight_common::high::MoonlightClientError) -> bool {
    use moonlight_common::high::MoonlightClientError as E;

    match err {
        E::Backend(inner) => matches!(
            inner.downcast_ref::<super::http::GsHttpError>(),
            Some(super::http::GsHttpError::Ureq(ureq::Error::Timeout(_)))
        ),
        _ => false,
    }
}

/// Same question for the stream setup: a handshake that timed out or lost its socket, as opposed to
/// a host that rejected the session.
///
/// The caller pairs this with "did this attempt load NDL": a setup that got as far as the video
/// stream already ran `SinkDecoder::setup`, and that decoder handle went down with the crate's
/// thread with nothing left to unload it — a second attempt would load one on top of it.
fn setup_unanswered(err: &MoonlightStreamError) -> bool {
    matches!(
        err,
        MoonlightStreamError::ConnectionTimeout | MoonlightStreamError::Io(_) | MoonlightStreamError::Proto(_)
    )
}

/// `/applist` entry to launch. `None` means Desktop, which on a `GameStream` host is an ordinary
/// app entry the host names — so it is resolved here rather than anywhere a screen can see.
fn resolve_app_id(host: &query::Host, app_id: Option<&str>) -> Result<AppId> {
    if let Some(id) = app_id {
        let id: u32 = id.parse().with_context(|| format!("not a GameStream app id: {id:?}"))?;
        return Ok(AppId(id));
    }
    let apps = query::app_list(host)?;
    apps.iter()
        .find(|a| crate::core::model::is_desktop_title(&a.title))
        .map(|a| a.id)
        // A host with no Desktop entry: the first app is a worse guess than an honest failure,
        // since it would silently launch a game the user didn't pick.
        .context("this host's app list has no Desktop entry")
}

/// The formats offered to the host. NDL decodes H.264 and HEVC only (no AV1 — see
/// `platform::webos::ndl`), and Main10 is what carries HDR.
fn supported_formats(codec: CodecPref, hdr: bool) -> VideoFormats {
    match codec {
        CodecPref::H264 => VideoFormats::H264,
        CodecPref::Hevc if hdr => VideoFormats::H265 | VideoFormats::H265_MAIN10,
        CodecPref::Hevc => VideoFormats::H265,
        CodecPref::Auto if hdr => VideoFormats::H264 | VideoFormats::H265 | VideoFormats::H265_MAIN10,
        CodecPref::Auto => VideoFormats::H264 | VideoFormats::H265,
    }
}

/// The bitrate for "Automatic". punktfunk hands that decision to the host's ABR controller;
/// `GameStream` has none (`BackendCaps::host_abr` is false), so the client picks — with
/// Moonlight's own resolution/fps table, scaled linearly between its anchor points so 1440p and
/// the 120 Hz modes this TV can drive aren't left at a 60 Hz figure.
fn default_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    // Moonlight's anchors, all at 60 fps: 720p 10, 1080p 20, 1440p 40, 4K 80 Mbps.
    let base_kbps: u32 = match width * height {
        p if p <= 1280 * 720 => 10_000,
        p if p <= 1920 * 1080 => 20_000,
        p if p <= 2560 * 1440 => 40_000,
        _ => 80_000,
    };
    // Same shape Moonlight uses above 60: linear in frame rate, not a second table.
    let scaled = u64::from(base_kbps) * u64::from(fps.max(30)) / 60;
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

/// The user-facing sentence for a failed host call; the technical form goes to the log — see
/// `http::api_message`.
fn gs_err(e: moonlight_common::high::MoonlightClientError) -> anyhow::Error {
    anyhow::anyhow!("{}", crate::backend::gamestream::http::api_message("launch", &e))
}

/// The crate's video callback, over this client's [`NdlSink`].
struct SinkDecoder {
    /// Built in `setup`, on the crate's video thread — see the module docs.
    sink: Option<NdlSink>,
    stats: Arc<StreamStats>,
    shared: Arc<Shared>,
    formats: VideoFormats,
    frames: u64,
    /// Previous frame number, for the loss flag the sink's freeze-until-reanchor reads.
    last_frame_index: Option<u32>,
    /// Access-unit scratch, reused per frame: the crate hands over a buffer chain and NDL takes
    /// one contiguous Annex B unit.
    au: Vec<u8>,
}

impl VideoDecoder for SinkDecoder {
    fn setup(&mut self, setup: VideoSetup) -> i32 {
        let codec = match setup.format {
            moonlight_common::stream::video::VideoFormat::H264
            | moonlight_common::stream::video::VideoFormat::H264High8_444 => NdlCodec::H264,
            moonlight_common::stream::video::VideoFormat::H265
            | moonlight_common::stream::video::VideoFormat::H265Main10
            | moonlight_common::stream::video::VideoFormat::H265Rext8_444
            | moonlight_common::stream::video::VideoFormat::H265Rext10_444 => NdlCodec::H265,
            other => {
                tracing::error!("GameStream negotiated {other:?}, which NDL cannot decode");
                return -1;
            }
        };
        let ndl = match NdlVideo::load(
            &crate::platform::webos::ndl::app_id(),
            setup.width as i32,
            setup.height as i32,
            codec,
            // Audio decodes in software (see the module docs on the channel); NDL's audio
            // offload is disabled on this client regardless — see `session::NDL_AUDIO_OFFLOAD`.
            None,
        ) {
            Ok(ndl) => Arc::new(ndl),
            Err(e) => {
                tracing::error!("GameStream NDL load failed: {e:#}");
                return -1;
            }
        };
        tracing::info!(
            "NDL loaded for GameStream ({codec:?} {}x{}@{}fps)",
            setup.width,
            setup.height,
            setup.redraw_rate,
        );
        if let Ok(mut panel) = self.shared.panel.lock() {
            panel.ndl = Some(ndl.clone());
            // The *negotiated* format decides HDR, same rule as `session::connect`: `NdlHdrInfo`'s
            // fields are HEVC SEI syntax, so no other codec can carry HDR on this platform.
            if codec != NdlCodec::H265 && panel.wanted.0 {
                tracing::info!("GameStream negotiated {codec:?}, which cannot carry HDR — staying SDR");
                panel.wanted = (false, None);
            }
        }
        // Now, not on the first host HDR packet: a host that sends none would otherwise stream HDR
        // content at a panel still in SDR, and one that sent its packet during the NDL load had
        // nothing to apply to. Same point in the sequence as `session::connect`'s own colour push.
        self.shared.apply_hdr();
        self.sink = Some(NdlSink::new(
            VideoPlayer::new(ndl),
            self.stats.clone(),
            SinkConfig {
                stream_hz: setup.redraw_rate,
                // Nothing to report to: `GameStream` has no client decode-latency channel.
                report_decode_latency: false,
                keyframe_min_interval: KEYFRAME_REQUEST_MIN_INTERVAL,
            },
        ));
        0
    }

    fn start(&mut self) {
        tracing::info!("GameStream video stream starting");
    }

    fn submit_decode_unit(&mut self, unit: VideoDecodeUnit<&[u8]>) -> DecodeResult {
        let Some(sink) = self.sink.as_mut() else {
            return DecodeResult::NeedIdr;
        };
        // A single buffer already IS the contiguous unit NDL wants — feeding it directly saves a
        // full-frame copy per frame, which at 120fps is the hot path. Only real chains concat.
        let au: &[u8] = if unit.buffers.len() == 1 {
            unit.buffers[0].data
        } else {
            self.au.clear();
            for buffer in &unit.buffers {
                self.au.extend_from_slice(buffer.data);
            }
            &self.au
        };
        self.frames += 1;
        self.stats.frames.store(self.frames, Ordering::Relaxed);
        self.stats.bytes.fetch_add(au.len() as u64, Ordering::Relaxed);

        let index = unit.frame_number.0;
        // The crate never delivers an incomplete frame, so a hole in the numbering is a frame
        // FEC couldn't recover — the same signal punktfunk's `note_frame_index` gap gives.
        let loss = self.last_frame_index.is_some_and(|prev| index != prev.wrapping_add(1));
        if loss {
            tracing::warn!("GameStream loss: frame {index} after {:?}", self.last_frame_index);
        }
        self.last_frame_index = Some(index);
        let flags = FrameFlags {
            reanchor: unit.frame_type == FrameType::Idr,
            loss,
            index: u64::from(index),
        };
        match sink.submit(au, pts_ns(&unit), flags) {
            // The crate turns `NeedIdr` into a `RequestIdr` control packet itself, which is why
            // the sink's throttle is what keeps it off the input channel's back.
            SinkResult::NeedKeyframe => DecodeResult::NeedIdr,
            SinkResult::Presented { .. } | SinkResult::Held => DecodeResult::Ok,
        }
    }

    fn stop(&mut self) {
        tracing::info!("GameStream video stream stopping after {} frames", self.frames);
        // Drops the sink's `VideoPlayer`, and with it one of the two `Arc<NdlVideo>` owners;
        // `GsStream::shutdown` releases the other, and waits for this flag before it does.
        self.sink = None;
        self.shared.video_stopped.store(true, Ordering::Relaxed);
    }

    fn supported_formats(&self) -> VideoFormats {
        self.formats
    }
}

/// The host's 90 kHz presentation timestamp in nanoseconds, which is the clock domain
/// [`NdlSink::submit`] anchors against.
fn pts_ns(unit: &VideoDecodeUnit<&[u8]>) -> u64 {
    u64::try_from(unit.timestamp.as_nanos()).unwrap_or(u64::MAX)
}

/// The crate's audio callback: hands Opus frames to the main thread, which owns the SDL device.
struct ChannelAudioDecoder {
    tx: mpsc::SyncSender<Vec<u8>>,
    /// Frames dropped because the main loop wasn't draining; logged in `stop` rather than per
    /// frame, since a stalled main thread does not need a log flood on top.
    dropped: u64,
}

impl AudioDecoder for ChannelAudioDecoder {
    fn setup(&mut self, audio_config: AudioConfig, stream_config: OpusMultistreamConfig) -> i32 {
        tracing::info!(
            "GameStream audio: {} channel(s), {} stream(s) ({} coupled), {} samples/frame",
            stream_config.channel_count,
            stream_config.streams,
            stream_config.coupled_streams,
            stream_config.samples_per_frame,
        );
        let channels = u8::try_from(audio_config.channel_count).unwrap_or(0);
        if channels != GsStream::AUDIO_CHANNELS {
            // The launch request asked for stereo, so this is a host ignoring it. Failing here
            // stops the stream with a reason instead of playing noise through a stereo decoder.
            tracing::error!("GameStream host negotiated {channels} channels; only stereo is supported");
            return -1;
        }
        0
    }

    fn start(&mut self) {}

    fn stop(&mut self) {
        if self.dropped > 0 {
            tracing::warn!(
                "GameStream audio: {} frame(s) dropped (main loop not draining)",
                self.dropped
            );
        }
    }

    fn decode_and_play_sample(&mut self, sample: AudioFrame<&[u8]>) {
        // `try_send` rather than `send`: blocking the crate's audio thread on a stalled main
        // loop would back the whole audio driver up behind it.
        if self.tx.try_send(sample.buffer.to_vec()).is_err() {
            self.dropped += 1;
        }
    }

    fn config(&self) -> AudioConfig {
        AudioConfig::STEREO
    }
}

/// The crate's control-stream callback: HDR, the lightbar, and knowing the session ended. Rumble
/// is absent because the crate never delivers it — see [`GsStream::pump_feedback_once`].
struct Listener {
    shared: Arc<Shared>,
}

impl ConnectionListener for Listener {
    fn set_hdr_mode(&mut self, enabled: bool, sunshine: Option<SunshineHdrMetadata>) {
        tracing::info!("GameStream HDR mode: enabled={enabled} metadata={}", sunshine.is_some());
        if let Ok(mut panel) = self.shared.panel.lock() {
            panel.wanted = (enabled, enabled.then_some(sunshine).flatten());
        }
        self.shared.apply_hdr();
    }

    fn controller_set_led(&mut self, controller_number: u16, r: u8, g: u8, b: u8) {
        // Queued rather than applied here: the DualSense report goes out on the main thread's
        // device (see `GsStream::pump_feedback_once`), and the control thread must not wait on it.
        self.shared.led.store(pack_led(r, g, b), Ordering::Relaxed);
        tracing::debug!("GameStream pad {controller_number} LED {r:02x}{g:02x}{b:02x}");
    }

    fn controller_set_motion_event_state(
        &mut self,
        controller_number: u16,
        motion_type: moonlight_common::stream::control::MotionType,
        report_rate_hz: u16,
    ) {
        // Logged, not honoured: this client sends no motion events, so a host asking for them at
        // a rate is asking for something no `InputEvent` here carries.
        tracing::debug!("GameStream pad {controller_number} wants {motion_type:?} at {report_rate_hz} Hz — not sent");
    }

    fn connection_terminated(&mut self, error_code: i32) {
        tracing::info!("GameStream session terminated (code {error_code})");
        self.shared.termination_code.store(error_code, Ordering::Relaxed);
        self.shared.terminated.store(true, Ordering::Relaxed);
    }
}

/// `GameStream`'s HDR metadata in the SEI-derived shape NDL takes.
///
/// The chromaticity fields are identical (1/50000 units, ST.2086 G/B/R order) but the **luminance
/// units differ**: `GameStream` sends max display luminance in whole nits and min in 1/10000 nits
/// (Sunshine mirrors the DXGI metadata layout there), while punktfunk's `HdrMeta` — and NDL's
/// struct behind it — is 1/10000 cd/m² for both. Passing the max through unscaled would tell the
/// panel the content was mastered at 0.08 nits.
fn hdr_meta(m: SunshineHdrMetadata) -> quic::HdrMeta {
    quic::HdrMeta {
        display_primaries: m.display_primaries.map(|p| [p.x, p.y]),
        white_point: [m.white_point.x, m.white_point.y],
        max_display_mastering_luminance: u32::from(m.max_display_luminance) * 10_000,
        min_display_mastering_luminance: u32::from(m.min_display_luminance),
        max_cll: m.max_content_light_level,
        max_fall: m.max_frame_average_light_level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The atomic replaced a `Mutex<Option<(u8, u8, u8)>>`, so the encoding is worth pinning:
    /// zero has to mean "nothing asked for", including for a genuine black lightbar request.
    #[test]
    fn led_packing_round_trips_and_reserves_zero() {
        assert_eq!(unpack_led(0), None);
        assert_eq!(unpack_led(pack_led(0, 0, 0)), Some((0, 0, 0)));
        assert_eq!(unpack_led(pack_led(1, 2, 3)), Some((1, 2, 3)));
        assert_eq!(unpack_led(pack_led(255, 128, 7)), Some((255, 128, 7)));
    }

    /// The Automatic bitrate is the one number no host tells us, so its shape is worth pinning:
    /// Moonlight's 60 Hz anchors, scaled linearly above that.
    #[test]
    fn automatic_bitrate_matches_moonlight_anchors_and_scales_with_fps() {
        assert_eq!(default_bitrate_kbps(1920, 1080, 60), 20_000);
        assert_eq!(default_bitrate_kbps(3840, 2160, 60), 80_000);
        assert_eq!(default_bitrate_kbps(2560, 1440, 120), 80_000);
    }

    /// AV1 is never offered (NDL can't decode it) and HDR must bring Main10 with it, or the host
    /// picks an 8-bit format and the HDR request quietly does nothing.
    #[test]
    fn offered_formats_exclude_av1_and_pair_hdr_with_main10() {
        let auto_hdr = supported_formats(CodecPref::Auto, true);
        assert!(auto_hdr.contains(VideoFormats::H265_MAIN10));
        assert!(!auto_hdr.intersects(VideoFormats::MASK_AV1));
        assert!(!supported_formats(CodecPref::H264, true).intersects(VideoFormats::MASK_H265));
    }
}
