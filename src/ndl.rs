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
/// embeds a `double`, giving the union 8-byte alignment — `align(8)` here matches that
/// even though we only ever store zeros (tag 0 = `NDL_AUDIO_TYPE` unset = "no audio").
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct NdlAudioUnion {
    bytes: [u8; 32],
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
}

impl NdlVideo {
    /// Loads NDL for a video stream of `codec` at `width`x`height`. Calls
    /// `NDL_DirectMediaInit` on first use. No audio is configured (see module docs).
    pub fn load(app_id: &str, width: i32, height: i32, codec: NdlCodec) -> Result<Self> {
        ensure_init(app_id)?;
        let mut info = NdlDataInfo {
            video: NdlVideoInfo {
                width,
                height,
                kind: codec.ndl_type(),
                unknown1: 0,
            },
            audio: NdlAudioUnion { bytes: [0; 32] },
        };
        // SAFETY: `info` is valid for the duration of this call.
        let ret = unsafe { NDL_DirectMediaLoad(&mut info, None) };
        if ret != 0 {
            bail!("NDL_DirectMediaLoad failed: ret={ret} error={}", last_error());
        }
        Ok(Self {
            load_instant: Instant::now(),
        })
    }

    /// Feed one access unit. The host's `pts_ns` is deliberately ignored — NDL wants
    /// milliseconds since `load`, not wall-clock or the host's capture clock, so the
    /// PTS is derived from `load_instant` instead (see the [`NdlVideo`] doc comment).
    pub fn play(&self, au: &[u8]) -> Result<()> {
        let pts_ms = self.load_instant.elapsed().as_millis() as c_longlong;
        // SAFETY: NDL reads `size` bytes from `buffer` synchronously and does not
        // retain the pointer.
        let ret = unsafe {
            NDL_DirectVideoPlay(au.as_ptr() as *mut c_void, au.len() as c_uint, pts_ms)
        };
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
        // SAFETY: passed by value; no pointers or aliasing.
        let ret = unsafe { NDL_DirectVideoSetHDRInfo(info) };
        if ret != 0 {
            bail!("NDL_DirectVideoSetHDRInfo failed: ret={ret} error={}", last_error());
        }
        Ok(())
    }

    /// Flush buffered-but-undisplayed frames — call after a keyframe request so
    /// stale frames don't head-of-line block the fresh one.
    pub fn flush(&self) -> Result<()> {
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
