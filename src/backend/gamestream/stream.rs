//! `GameStream` streaming: `/launch` plus the crate's blocking stream driver, wired to this
//! client's own video sink and audio device.
//!
//! **No hand-rolled transport.** `docs/GameStream-Plan.md`'s P4 assumed we would drive the sans-IO
//! core ourselves — UDP sockets, the 500 ms video/audio pings, the 100 ms control ping,
//! `poll_output` dispatch. `moonlight_common::stream::std::MoonlightStream` already is that
//! driver: it owns the RTSP handshake, the three UDP drivers, the `ENet` control peer and their
//! ping obligations, and calls back into [`VideoDecoder`]/[`AudioDecoder`]/[`ConnectionListener`]
//! on its own threads. So this module is the three callback implementations plus the settings
//! mapping, and the protocol's timing stays in the crate that has tests for it.
//!
//! Thread shape, which is what the rest of the design follows from:
//!
//! * **video** — the crate's video thread calls [`SinkDecoder`], which owns [`NdlSink`] exactly as
//!   punktfunk's `video_pump` thread does. NDL is loaded in `setup`, on that thread, for the same
//!   reason `session::connect` builds the sink on the pump thread: the pacer queries the panel
//!   refresh through SDL on construction.
//! * **audio** — the crate's audio thread cannot touch `AudioPlayer`: `sdl2::audio::AudioQueue` is
//!   `!Send` and lives on the main thread. So Opus frames cross to the main loop over a bounded
//!   channel, drained by [`GsStream::pump_audio_once`] on the same tick punktfunk's software path
//!   uses.
//! * **control** — HDR changes arrive on the control thread and go straight to the shared
//!   `Arc<NdlVideo>`, which has its own FFI mutex.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::stream::audio::{AudioConfig, AudioDecoder, AudioFrame, OpusMultistreamConfig};
use moonlight_common::stream::connection::ConnectionListener;
use moonlight_common::stream::control::ActiveGamepads;
use moonlight_common::stream::proto::control::input_batcher::ClientInputEvent;
use moonlight_common::stream::std::MoonlightStream;
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
use crate::session::sink::{FrameFlags, NdlSink, SinkConfig, SinkResult, VideoPlayer, VideoSink};
use crate::session::StreamStats;

/// Bytes per video packet on the wire. Moonlight's own default, and the value Sunshine is tuned
/// for; it is not a user setting on any client.
const PACKET_SIZE: u32 = 1392;

/// IDR requests share the `ENet` control channel with gamepad input, so an unthrottled request
/// loop directly inflates input latency — the whole subject of aurora-tv's
/// `idr-throttle-input-latency` patch. Ten times punktfunk's interval, which pays nothing for a
/// request on its own QUIC stream (see `sink::KEYFRAME_REQUEST_MIN_INTERVAL`).
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Opus frames buffered between the crate's audio thread and the main loop's drain. At
/// `GameStream`'s 20 ms framing that is 640 ms of audio — far more than the ~2 ms tick needs, so
/// the bound is only reached if the main thread stalls, and then dropping the oldest frames is
/// what the software path does anyway (see `audio::SOFT_QUEUED_LAG_MS`).
const AUDIO_QUEUE_FRAMES: usize = 32;

/// Ceiling on waiting for the video callback to stop during teardown; see [`GsStream::shutdown`].
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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

/// State the three callback threads and the main loop all touch.
struct Shared {
    /// Set once the video decoder has loaded NDL, so the control thread's HDR updates have
    /// something to apply to. `Mutex` rather than `OnceLock` because it is also cleared on stop.
    ndl: Mutex<Option<Arc<NdlVideo>>>,
    /// Last HDR state applied to the panel. Re-applying `NDL_DirectVideoSetHDRInfo` on every host
    /// HDR packet re-enters panel HDR mode and drops a 120 Hz panel to 60 (see
    /// `docs/NOTES.md` and the CX finding) — so this dedupes, exactly as `session/mod.rs` does.
    hdr_applied: Mutex<Option<SunshineHdrMetadata>>,
    /// The host ended the session (`ServerTermination`, or the control stream disconnecting).
    terminated: AtomicBool,
    /// `ServerTermination`'s reason code, 0 when the host said the user closed the app.
    termination_code: AtomicI32,
    /// Channel count the host actually negotiated, published by the audio decoder's `setup`.
    audio_channels: AtomicU8,
    /// The video callback has run its `stop` and released its NDL handle — see
    /// [`GsStream::shutdown`].
    video_stopped: AtomicBool,
    color_range_override: ColorRangeOverride,
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

    /// Channels the host negotiated — build [`crate::platform::webos::audio::AudioPlayer`] from
    /// this, never from the request (same rule as punktfunk's `client.audio_channels`).
    pub fn audio_channels(&self) -> u8 {
        self.shared.audio_channels.load(Ordering::Relaxed)
    }

    /// The host ended the session, or the transport dropped.
    pub fn is_session_ended(&self) -> bool {
        self.shared.terminated.load(Ordering::Relaxed) || self.stream.is_stopped()
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
        // sink and with it one of the two `Arc<NdlVideo>` owners. Bounded for the same reason
        // `session::SHUTDOWN_JOIN_TIMEOUT` is: the FFI calls between the driver's own stop checks
        // have no timeout of their own, and a wedged vendor call must not freeze the app.
        let deadline = std::time::Instant::now() + SHUTDOWN_TIMEOUT;
        while !self.shared.video_stopped.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let clean = self.shared.video_stopped.load(Ordering::Relaxed);
        if !clean {
            tracing::error!("GameStream video callback did not stop within {SHUTDOWN_TIMEOUT:?} — leaking it");
        }
        // Our own handle goes last: while any `Arc<NdlVideo>` lives, the decoder stays loaded, so
        // releasing it only here is what keeps `play` from racing the unload.
        if let Ok(mut slot) = self.shared.ndl.lock() {
            drop(slot.take());
        }
        clean
    }
}

/// Connects, launches (or resumes) the app, and starts the stream. Blocking — call it on the
/// connect thread, like `session::connect`.
pub fn connect(spec: GsConnectSpec) -> Result<GsStream> {
    let host = query::open(&spec.addr, Some(spec.query_port)).context("open GameStream host")?;
    if !host.is_paired().unwrap_or(false) {
        anyhow::bail!("this host hasn't been paired yet");
    }

    let app_id = resolve_app_id(&host, spec.app_id.as_deref())?;
    let hdr = spec.hdr_enabled && spec.codec != CodecPref::H264;
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
        hdr,
        supported_video_formats: supported_formats(spec.codec, hdr),
        color_space: if hdr { ColorSpace::Rec2020 } else { ColorSpace::Rec709 },
        color_range: match spec.color_range_override {
            // HDR is always limited range; the override only speaks for SDR.
            _ if hdr => ColorRange::Limited,
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

    let crypto = RustCryptoBackend;
    let aes_key = AesKey::new_random(&crypto).map_err(|e| anyhow::anyhow!("aes key: {e}"))?;
    let aes_iv = AesIv::new_random(&crypto).map_err(|e| anyhow::anyhow!("aes iv: {e}"))?;
    let config = host
        .start_stream(
            app_id,
            &settings,
            aes_key,
            aes_iv,
            MoonlightStream::launch_query_parameters(),
        )
        .map_err(gs_err)
        .context("launch")?;
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
        ndl: Mutex::new(None),
        hdr_applied: Mutex::new(None),
        terminated: AtomicBool::new(false),
        termination_code: AtomicI32::new(0),
        // Until the host says otherwise, what we asked for.
        audio_channels: AtomicU8::new(2),
        video_stopped: AtomicBool::new(false),
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
        shared: shared.clone(),
        dropped: 0,
    };
    let listener = Listener { shared: shared.clone() };

    let bitrate_kbps = settings.bitrate;
    let stream = MoonlightStream::connect(config, settings, video, audio, listener, Arc::new(crypto) as _)
        .map_err(|e| anyhow::anyhow!("start GameStream stream: {e}"))?;
    tracing::info!("GameStream host features: {:?}", stream.host_features());
    let stream = Arc::new(stream);

    Ok(GsStream {
        input: Arc::new(GsInput {
            stream: stream.clone(),
            state: Mutex::new(InputState {
                translator: InputTranslator::new(spec.gamepad_type),
                out: Vec::new(),
            }),
        }),
        stream,
        stats,
        shared,
        audio_rx,
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

/// `/applist` entry to launch. `None` means Desktop, which on a `GameStream` host is an ordinary
/// app entry the host names — so it is resolved here rather than anywhere a screen can see.
fn resolve_app_id(host: &query::Host, app_id: Option<&str>) -> Result<AppId> {
    if let Some(id) = app_id {
        let id: u32 = id.parse().with_context(|| format!("not a GameStream app id: {id:?}"))?;
        return Ok(AppId(id));
    }
    let apps = query::app_list(host)?;
    apps.iter()
        .find(|a| a.title.eq_ignore_ascii_case("desktop"))
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

fn gs_err(e: moonlight_common::high::MoonlightClientError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
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
        let app_id = std::env::var("APPID").unwrap_or_else(|_| "io.dyptan.punktfunk.webos".into());
        let ndl = match NdlVideo::load(
            &app_id,
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
        if let Ok(mut slot) = self.shared.ndl.lock() {
            *slot = Some(ndl.clone());
        }
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
        self.au.clear();
        for buffer in &unit.buffers {
            self.au.extend_from_slice(buffer.data);
        }
        self.frames += 1;
        self.stats.frames.store(self.frames, Ordering::Relaxed);
        self.stats.bytes.fetch_add(self.au.len() as u64, Ordering::Relaxed);

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
        match sink.submit(&self.au, pts_ns(&unit), flags) {
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
/// [`VideoSink::submit`] anchors against.
fn pts_ns(unit: &VideoDecodeUnit<&[u8]>) -> u64 {
    u64::try_from(unit.timestamp.as_nanos()).unwrap_or(u64::MAX)
}

/// The crate's audio callback: hands Opus frames to the main thread, which owns the SDL device.
struct ChannelAudioDecoder {
    tx: mpsc::SyncSender<Vec<u8>>,
    shared: Arc<Shared>,
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
        let channels = u8::try_from(audio_config.channel_count).unwrap_or(2);
        if channels != 2 {
            // The launch request asked for stereo, so this is a host ignoring it. Failing here
            // stops the stream with a reason instead of playing noise through a stereo decoder.
            tracing::error!("GameStream host negotiated {channels} channels; only stereo is supported");
            return -1;
        }
        self.shared.audio_channels.store(channels, Ordering::Relaxed);
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

/// The crate's control-stream callback. Rumble and the other pad feedback are P5 (see the plan);
/// what matters here is HDR and knowing the session ended.
struct Listener {
    shared: Arc<Shared>,
}

impl ConnectionListener for Listener {
    fn set_hdr_mode(&mut self, enabled: bool, sunshine: Option<SunshineHdrMetadata>) {
        tracing::info!("GameStream HDR mode: enabled={enabled} metadata={}", sunshine.is_some());
        // Dedupe before touching NDL: see `Shared::hdr_applied`.
        let Ok(mut applied) = self.shared.hdr_applied.lock() else {
            return;
        };
        let wanted = enabled.then_some(sunshine).flatten();
        if *applied == wanted {
            return;
        }
        *applied = wanted;
        let Ok(slot) = self.shared.ndl.lock() else {
            return;
        };
        let Some(ndl) = slot.as_ref() else {
            return;
        };
        // `set_color_info` is a no-op without metadata, which is the wanted behaviour for
        // HDR-off: an SDR stream must not emit an HDR infoframe at all (see `ndl.rs`).
        let meta = wanted.map(hdr_meta);
        let mut color = if enabled {
            quic::ColorInfo::HDR10_BT2020_PQ
        } else {
            quic::ColorInfo::SDR_BT709
        };
        self.shared.color_range_override.apply(&mut color);
        if let Err(e) = ndl.set_color_info(meta.as_ref(), color) {
            tracing::warn!("NDL set_color_info: {e:#}");
        }
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
