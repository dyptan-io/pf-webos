//! Safe wrapper over webOS's NDL `DirectMedia` v2 API (`NDL_Direct*`, webOS 5+).
//!
//! We only use the VIDEO half. Audio goes through SDL2 (`audio.rs`), never NDL —
//! `NdlDataInfo.audio` is always zeroed (tag 0 = none), which NDL accepts as long as
//! `video.type` is set (confirmed in ss4s's `ndl_player.c`).
//!
//! Deliberately never calls `NDL_DirectVideoSetArea`: ss4s's webOS 5 NDL module
//! (`ndl_video.c`) doesn't either, letting NDL's own default punch-through mapping
//! handle any decode resolution. Forcing an explicit rect sized from
//! `SDL_GetCurrentDisplayMode` (which reports a fixed 1080p compositor resolution on
//! this TV, not the physical panel size) made NDL scale every frame down into that
//! rect above 1080p, causing resolution-triggered stutter independent of bitrate/fps.
use std::ffi::{c_char, c_int, c_longlong, c_uint, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{bail, Result};

#[repr(C)]
#[derive(Clone, Copy)]
struct NdlVideoInfo {
    width: c_int,
    height: c_int,
    /// `NDL_VIDEO_TYPE`: 1=H264, 2=H265, 3=VP9, 4=AV1.
    kind: c_int,
    unknown1: c_int,
}

/// The real C type is a union whose largest arm (`NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T`)
/// embeds a `double`, giving the union 8-byte alignment — `align(8)` matches that. Tag 0
/// (`NDL_AUDIO_TYPE` unset) means "no audio", which is what an all-zero value encodes.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct NdlAudioUnion {
    bytes: [u8; 32],
}

/// `NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T`, field-for-field. Written into
/// [`NdlAudioUnion`]'s 32 bytes — the header's own `char padding[32]` arm confirms that
/// size, and on this 32-bit target the `double` at offset 16 is what forces the union's
/// 8-byte alignment.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct NdlAudioOpusInfo {
    /// `NDL_AUDIO_TYPE`: 3 = OPUS.
    kind: c_int,
    unknown1: c_int,
    channels: c_int,
    unknown2: c_int,
    sample_rate: f64,
    /// `const char *streamHeader`. Undocumented beyond its type; passed null, which is
    /// why `load` treats an audio-enabled load as something to fall back from rather
    /// than to rely on. See [`NdlVideo::load`].
    stream_header: *const c_char,
}

/// Audio to hand to NDL alongside the video stream, or `None` to keep NDL video-only and
/// decode audio in-process.
///
/// Opus **only**, and stereo only in practice: `NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T` carries
/// a channel count and a sample rate and nothing else — there is no multistream mapping
/// field — so it cannot describe the 5.1/7.1 layouts punktfunk negotiates through
/// `punktfunk_core::audio::layout_for`. Those must stay on the software path.
#[derive(Clone, Copy)]
pub struct NdlAudioConfig {
    pub channels: i32,
    pub sample_rate: f64,
}

impl NdlAudioConfig {
    fn to_union(self) -> NdlAudioUnion {
        let info = NdlAudioOpusInfo {
            kind: 3, // NDL_AUDIO_TYPE_OPUS
            unknown1: 0,
            channels: self.channels as c_int,
            unknown2: 0,
            sample_rate: self.sample_rate,
            stream_header: std::ptr::null(),
        };
        let mut bytes = [0u8; 32];
        // SAFETY: `NdlAudioOpusInfo` is `repr(C)` and no larger than the union's 32-byte
        // arm (the header's own `char padding[32]`), so this copy stays in bounds. Any
        // trailing bytes remain zero, matching the C compiler's own padding.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&info).cast::<u8>(),
                bytes.as_mut_ptr(),
                std::mem::size_of::<NdlAudioOpusInfo>().min(32),
            );
        }
        NdlAudioUnion { bytes }
    }
}

#[repr(C)]
struct NdlDataInfo {
    video: NdlVideoInfo,
    audio: NdlAudioUnion,
}

/// `NDL_VIDEO_TYPE` values this client can request (matches the codec the host's
/// `Welcome` resolved — see `punktfunk_core::quic::CODEC_*`).
#[derive(Clone, Copy, Debug)]
pub enum NdlCodec {
    H264,
    H265,
    Av1,
}

impl NdlCodec {
    fn ndl_type(self) -> c_int {
        match self {
            Self::H264 => 1,
            Self::H265 => 2,
            Self::Av1 => 4,
        }
    }

    /// From a `punktfunk_core::quic::CODEC_*` wire bit (the resolved `Welcome::codec`).
    /// NDL has no VP9 use here (punktfunk never emits it) and AV1 support depends on
    /// the TV's silicon — the caller decides whether to even negotiate it.
    pub fn from_wire(codec: u8) -> Option<Self> {
        match codec {
            punktfunk_core::quic::CODEC_H264 => Some(Self::H264),
            punktfunk_core::quic::CODEC_HEVC => Some(Self::H265),
            punktfunk_core::quic::CODEC_AV1 => Some(Self::Av1),
            _ => None,
        }
    }
}

/// Mirrors `NDL_DIRECTVIDEO_HDR_INFO_T` field-for-field — the field names are the
/// H.265 `mastering_display_colour_volume`/`content_light_level_info` SEI syntax
/// element names verbatim, so punktfunk's own `HdrMeta` (same SEI-derived fields,
/// same units) copies straight across with no unit conversion.
#[repr(C)]
struct NdlHdrInfo {
    display_primaries_x0: c_int,
    display_primaries_y0: c_int,
    display_primaries_x1: c_int,
    display_primaries_y1: c_int,
    display_primaries_x2: c_int,
    display_primaries_y2: c_int,
    white_point_x: c_int,
    white_point_y: c_int,
    max_display_mastering_luminance: c_int,
    min_display_mastering_luminance: c_int,
    max_content_light_level: c_int,
    max_pic_average_light_level: c_int,
    transfer_characteristics: c_int,
    color_primaries: c_int,
    matrix_coeffs: c_int,
    reserved: [u8; 32],
}

#[allow(non_camel_case_types)]
type ResourceReleased = Option<extern "C" fn(*const c_char)>;
#[allow(non_camel_case_types)]
type NdlMediaLoadCallback = Option<extern "C" fn(c_int, c_longlong, *const c_char)>;

#[link(name = "NDL_directmedia")]
extern "C" {
    fn NDL_DirectMediaGetError() -> *const c_char;
    fn NDL_DirectMediaInit(app_id: *const c_char, cb: ResourceReleased) -> c_int;
    fn NDL_DirectMediaQuit() -> c_int;
    fn NDL_DirectMediaLoad(data: *mut NdlDataInfo, cb: NdlMediaLoadCallback) -> c_int;
    fn NDL_DirectMediaUnload() -> c_int;
    fn NDL_DirectVideoPlay(buffer: *mut c_void, size: c_uint, pts: c_longlong) -> c_int;
    fn NDL_DirectVideoFlushRenderBuffer() -> c_int;
    fn NDL_DirectVideoGetRenderBufferLength(length: *mut c_int) -> c_int;
    fn NDL_DirectVideoSetFrameDropThreshold(threshold: c_int) -> c_int;
    fn NDL_DirectAudioPlay(buffer: *mut c_void, size: c_uint, pts: c_longlong) -> c_int;
    fn NDL_DirectVideoSetHDRInfo(hdr_info: NdlHdrInfo) -> c_int;
}

/// Reads NDL's last error string (set on the most recent failing call).
fn last_error() -> String {
    // SAFETY: returns a pointer to NDL's internal buffer; only borrowed here.
    unsafe {
        let p = NDL_DirectMediaGetError();
        if p.is_null() {
            "(no NDL error message)".to_string()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// Calls `NDL_DirectMediaInit` once (process-global, idempotent-guarded).
fn ensure_init(app_id: &str) -> Result<()> {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let c_app_id = CString::new(app_id).unwrap_or_default();
    // SAFETY: `c_app_id` is valid for the duration of this call.
    let ret = unsafe { NDL_DirectMediaInit(c_app_id.as_ptr(), None) };
    if ret != 0 {
        INIT_DONE.store(false, Ordering::SeqCst);
        bail!("NDL_DirectMediaInit failed: ret={ret} error={}", last_error());
    }
    Ok(())
}

/// One loaded NDL video decode session. Dropping it unloads (but does not call
/// `NDL_DirectMediaQuit` — that's process-global; see [`quit`]).
pub struct NdlVideo {
    /// NDL's PTS is milliseconds since `NDL_DirectMediaLoad`, not wall-clock or the
    /// host's capture clock (docs/NOTES.md) — NDL only needs a monotonically
    /// increasing local clock for its own internal pacing, so `play` derives it from
    /// this instead of the host-supplied timestamp.
    load_instant: Instant,
    /// Set when the load that succeeded was the audio-enabled one — see [`Self::load`].
    audio_offloaded: bool,
    /// Serializes every `NDL_Direct*` call in this session.
    ///
    /// NDL `DirectMedia` is a **singleton C API** — `NDL_DirectVideoPlay` and
    /// `NDL_DirectAudioPlay` take no context handle, so there is one implicit pipeline
    /// per process — and nothing in the SDK header documents it as thread-safe. Until
    /// the audio-offload drain moved to its own thread (`session::ndl_audio_pump`) both
    /// planes were fed from the video pump thread, so that question never arose; now
    /// two threads call in, and "undocumented" has to be read as "not safe".
    ///
    /// Cheap by construction: an uncontended lock is a couple of atomics against a
    /// `play()` that decodes *and* presents a frame, and the only waiter is a 5 ms audio
    /// packet against a feed that finishes in ~1 ms (`FEED_BACKPRESSURE_WARN` treats
    /// 20 ms as pathological). Far cheaper than the ≤500 ms head-of-line stall the
    /// dedicated audio thread exists to remove.
    ffi: std::sync::Mutex<()>,
}

impl NdlVideo {
    /// Loads NDL for a video stream of `codec` at `width`x`height`, optionally asking it
    /// to decode the Opus audio stream too. Calls `NDL_DirectMediaInit` on first use.
    ///
    /// **The audio request is a probe, not an assumption.** `NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T`
    /// is declared by the webOS 5+ header, but whether a given model's
    /// `libNDL_directmedia_impl` actually implements the Opus path is not something the
    /// header can tell us — and this binary ships to models neither developer owns. So an
    /// audio-enabled load that fails is retried immediately as video-only, and
    /// [`NdlVideo::audio_offloaded`] reports which one succeeded. That is a capability
    /// probe by attempt, which works on a TV that doesn't exist yet; a model allow-list
    /// would not.
    ///
    /// Caveat worth knowing: this can only detect a load that *fails*. If a device
    /// accepts the config and then produces no sound, that is silent — which is why the
    /// selection is logged loudly by the caller.
    pub fn load(app_id: &str, width: i32, height: i32, codec: NdlCodec, audio: Option<NdlAudioConfig>) -> Result<Self> {
        ensure_init(app_id)?;
        let video = NdlVideoInfo {
            width,
            height,
            kind: codec.ndl_type(),
            unknown1: 0,
        };
        if let Some(audio) = audio {
            let mut info = NdlDataInfo {
                video,
                audio: audio.to_union(),
            };
            // SAFETY: `info` is valid for the duration of this call.
            let ret = unsafe { NDL_DirectMediaLoad(&mut info, None) };
            if ret == 0 {
                return Ok(Self {
                    load_instant: Instant::now(),
                    audio_offloaded: true,
                    ffi: std::sync::Mutex::new(()),
                });
            }
            // Fall through to a video-only load rather than failing the session: audio
            // offload is an optimisation, and losing the stream over it would be a poor
            // trade.
            //
            // Unload first. A failed `NDL_DirectMediaLoad` is NOT documented to leave
            // nothing behind, and underneath it the platform acquires the decoder
            // through decproxy by resource permissions (docs/NOTES.md) — so a load that
            // got far enough to take those resources and then failed would make the
            // retry below fail too, turning "this TV doesn't do Opus offload" into "this
            // TV can't stream at all". Return ignored: there may be nothing to unload,
            // and an error here says nothing about whether the retry will work.
            tracing::warn!(
                "NDL audio-enabled load failed (ret={ret} error={}) — retrying video-only",
                last_error()
            );
            // SAFETY: no arguments; unloads at most the load attempted just above.
            let _ = unsafe { NDL_DirectMediaUnload() };
        }
        let mut info = NdlDataInfo {
            video,
            audio: NdlAudioUnion { bytes: [0; 32] },
        };
        // SAFETY: `info` is valid for the duration of this call.
        let ret = unsafe { NDL_DirectMediaLoad(&mut info, None) };
        if ret != 0 {
            bail!("NDL_DirectMediaLoad failed: ret={ret} error={}", last_error());
        }
        Ok(Self {
            load_instant: Instant::now(),
            audio_offloaded: false,
            ffi: std::sync::Mutex::new(()),
        })
    }

    /// Whether NDL accepted the Opus config and is decoding audio itself — if false the
    /// caller must run the software Opus path.
    pub fn audio_offloaded(&self) -> bool {
        self.audio_offloaded
    }

    /// Hands one raw Opus packet to NDL. Only valid when [`Self::audio_offloaded`].
    ///
    /// PTS is milliseconds since load, exactly as [`Self::play`] does for video — feeding
    /// both planes off the same clock is what lets NDL sync them, which is the part of
    /// this that the software path cannot do (it resnaps its own queue instead; see
    /// `audio::MAX_QUEUED_LAG_MS`).
    pub fn play_audio(&self, packet: &[u8]) -> Result<()> {
        let pts_ms = self.load_instant.elapsed().as_millis() as c_longlong;
        let _ffi = self.ffi.lock().expect("NDL FFI mutex poisoned");
        // SAFETY: NDL reads `size` bytes synchronously and does not retain the pointer.
        let ret = unsafe { NDL_DirectAudioPlay(packet.as_ptr() as *mut c_void, packet.len() as c_uint, pts_ms) };
        if ret != 0 {
            bail!("NDL_DirectAudioPlay failed: ret={ret} error={}", last_error());
        }
        Ok(())
    }

    /// Feed one access unit. The host's `pts_ns` is deliberately ignored — NDL wants
    /// milliseconds since `load`, not wall-clock or the host's capture clock, so the
    /// PTS is derived from `load_instant` instead (see the [`NdlVideo`] doc comment).
    pub fn play(&self, au: &[u8]) -> Result<()> {
        let pts_ms = self.load_instant.elapsed().as_millis() as c_longlong;
        let _ffi = self.ffi.lock().expect("NDL FFI mutex poisoned");
        // SAFETY: NDL reads `size` bytes from `buffer` synchronously and does not
        // retain the pointer.
        let ret = unsafe { NDL_DirectVideoPlay(au.as_ptr() as *mut c_void, au.len() as c_uint, pts_ms) };
        if ret != 0 {
            bail!("NDL_DirectVideoPlay failed: ret={ret} error={}", last_error());
        }
        Ok(())
    }

    /// Apply HDR mastering metadata. `meta` and `color` use the same SEI-standard
    /// units NDL expects (G/B/R order per ST.2086), so no conversion is needed.
    /// Forwards the stream's colorimetry (and, for HDR, its mastering metadata)
    /// to NDL. `meta: None` = an SDR stream: the mastering/light-level fields are
    /// zeroed (the SEI "unknown" convention) and only the colour triplet
    /// (transfer/primaries/matrix) is meaningful — without it, a bitstream with
    /// missing/unspecified VUI colour info leaves the panel to guess colorimetry
    /// from resolution, and a 4K SDR stream then decodes as BT.2020 instead of
    /// the BT.709 punktfunk actually encodes (a visibly washed-out picture).
    pub fn set_color_info(
        &self,
        meta: Option<&punktfunk_core::quic::HdrMeta>,
        color: punktfunk_core::quic::ColorInfo,
    ) -> Result<()> {
        // G/B/R order (ST.2086 convention; same as starfish.rs).
        let ([g, b, r], white, max_dml, min_dml, cll, fall) = match meta {
            Some(m) => (
                m.display_primaries,
                m.white_point,
                m.max_display_mastering_luminance,
                m.min_display_mastering_luminance,
                m.max_cll,
                m.max_fall,
            ),
            None => ([[0; 2]; 3], [0; 2], 0, 0, 0, 0),
        };
        let info = NdlHdrInfo {
            display_primaries_x0: c_int::from(g[0]),
            display_primaries_y0: c_int::from(g[1]),
            display_primaries_x1: c_int::from(b[0]),
            display_primaries_y1: c_int::from(b[1]),
            display_primaries_x2: c_int::from(r[0]),
            display_primaries_y2: c_int::from(r[1]),
            white_point_x: c_int::from(white[0]),
            white_point_y: c_int::from(white[1]),
            max_display_mastering_luminance: max_dml as c_int,
            min_display_mastering_luminance: min_dml as c_int,
            max_content_light_level: c_int::from(cll),
            max_pic_average_light_level: c_int::from(fall),
            transfer_characteristics: c_int::from(color.transfer),
            color_primaries: c_int::from(color.primaries),
            matrix_coeffs: c_int::from(color.matrix),
            reserved: [0; 32],
        };
        let _ffi = self.ffi.lock().expect("NDL FFI mutex poisoned");
        // SAFETY: passed by value; no pointers or aliasing.
        let ret = unsafe { NDL_DirectVideoSetHDRInfo(info) };
        if ret != 0 {
            bail!("NDL_DirectVideoSetHDRInfo failed: ret={ret} error={}", last_error());
        }
        Ok(())
    }

    /// Flush buffered-but-undisplayed frames — call after a keyframe request so
    /// stale frames don't head-of-line block the fresh one.
    /// How much undecoded/unpresented video NDL is still holding.
    ///
    /// This is the signal this client has never had. `NDL_DirectVideoPlay` decodes *and*
    /// presents in one opaque call (see the module docs — it's why the shared
    /// `ReanchorGate` can't be used here), so "the decoder is falling behind" and "frames
    /// are arriving late" were indistinguishable from the outside: a slow `play()` call
    /// could mean either. A rising buffer length means the decoder is behind; a flat one
    /// near zero while frames stutter means the problem is upstream of it.
    ///
    /// `None` if the call fails — treated as "no reading", never as zero, so a failing
    /// query can't be mistaken for an empty queue.
    pub fn render_buffer_length(&self) -> Option<i32> {
        let mut length: c_int = 0;
        let _ffi = self.ffi.lock().expect("NDL FFI mutex poisoned");
        // SAFETY: `length` is a valid, writable `c_int` for the duration of the call.
        let ret = unsafe { NDL_DirectVideoGetRenderBufferLength(&mut length) };
        (ret == 0).then_some(length)
    }

    /// Sets NDL's internal frame-drop threshold.
    ///
    /// Units are undocumented (the SDK header declares the function and nothing else), so
    /// this is deliberately NOT called with a guessed value — `docs/NOTES.md` is explicit
    /// about not shipping unverified pacing changes to this decoder. It's wired to an
    /// optional on-device override file instead so the value can be swept against real
    /// playback without a rebuild; see `store::dev_override_ndl_drop_threshold`.
    pub fn set_frame_drop_threshold(threshold: i32) -> Result<()> {
        // SAFETY: plain integer argument, no pointers.
        let ret = unsafe { NDL_DirectVideoSetFrameDropThreshold(threshold as c_int) };
        if ret != 0 {
            bail!(
                "NDL_DirectVideoSetFrameDropThreshold({threshold}) failed: ret={ret} error={}",
                last_error()
            );
        }
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        let _ffi = self.ffi.lock().expect("NDL FFI mutex poisoned");
        // SAFETY: no arguments.
        let ret = unsafe { NDL_DirectVideoFlushRenderBuffer() };
        if ret != 0 {
            bail!(
                "NDL_DirectVideoFlushRenderBuffer failed: ret={ret} error={}",
                last_error()
            );
        }
        Ok(())
    }
}

impl Drop for NdlVideo {
    fn drop(&mut self) {
        // SAFETY: best-effort teardown; error ignored (Drop can't propagate a Result).
        let _ = unsafe { NDL_DirectMediaUnload() };
    }
}

/// Process-wide NDL teardown — call once at exit, after every `NdlVideo` has dropped.
pub fn quit() {
    if INIT_DONE.swap(false, Ordering::SeqCst) {
        // SAFETY: no arguments.
        unsafe {
            NDL_DirectMediaQuit();
        }
    }
}
