//! Safe wrapper over webOS's NDL `DirectMedia` v2 API (`NDL_Direct*`, webOS 5+) — the
//! system's hardware video decode path for native apps. Reverse-engineered but
//! well-established (the same API `mariotaku/moonlight-tv`/aurora-tv use); headers
//! confirmed against `libNDL_directmedia.so.1` live on an LG CX (webOS 5.6): every
//! symbol we call exists with a matching signature (checked via `nm -D`).
//!
//! We only use the VIDEO half. Audio is pre-decoded Opus→PCM in this client and goes
//! straight to SDL2 (`audio.rs`), never through NDL — so the `audio` union in
//! [`NdlDataInfo`] is always zeroed (`NDL_AUDIO_TYPE` tag 0 = none), which the
//! reference implementation (ss4s's `ndl_player.c`) confirms NDL accepts as long as
//! `video.type` is set.
//!
//! PTS is milliseconds since [`NdlVideo::load`] — not wall-clock, not the host's
//! capture clock — matching the reference `GetPts()` (`ndl_player.c`): NDL only needs
//! a monotonically increasing local clock for its own internal pacing.
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
    fn NDL_DirectVideoSetArea(left: c_int, top: c_int, width: c_int, height: c_int) -> c_int;
    fn NDL_DirectVideoFlushRenderBuffer() -> c_int;
    fn NDL_DirectVideoSetHDRInfo(hdr_info: NdlHdrInfo) -> c_int;
}

/// Reads NDL's last error string (set on the most recent failing call).
fn last_error() -> String {
    // SAFETY: NDL_DirectMediaGetError returns a pointer to a static/internal buffer
    // NDL owns; we only borrow it for the duration of the CStr read below.
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

/// `NDL_DirectMediaInit` is process-global and idempotent-guarded on NDL's side, but we
/// still only want to call it once. `app_id` matches `APPID` in the launched app's own
/// environment (confirmed present — see `punktfunk_webos_client` memory notes).
fn ensure_init(app_id: &str) -> Result<()> {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let c_app_id = CString::new(app_id).unwrap_or_default();
    // SAFETY: app_id points to a valid, live CString for the duration of this call;
    // the callback is null (we don't need load-state notifications for phase 1).
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
        // SAFETY: `info` is a valid, correctly-laid-out NDL_DIRECTMEDIA_DATA_INFO_T on
        // the stack for the duration of this call; the load-complete callback is null
        // (we don't block on it — NDL accepts NDL_DirectVideoPlay immediately after a
        // successful Load call, per the reference ndl_player.c sequencing).
        let ret = unsafe { NDL_DirectMediaLoad(&mut info, None) };
        if ret != 0 {
            bail!("NDL_DirectMediaLoad failed: ret={ret} error={}", last_error());
        }
        Ok(Self {
            load_instant: Instant::now(),
        })
    }

    /// Sets the punch-through video plane's rectangle in panel pixels. Call once after
    /// `load` (and again on a display-mode/resize change) — the video plane is
    /// composited by the display hardware independent of any SDL2 window.
    pub fn set_area(&self, left: i32, top: i32, width: i32, height: i32) -> Result<()> {
        // SAFETY: plain integer args, no aliasing concerns.
        let ret = unsafe { NDL_DirectVideoSetArea(left, top, width, height) };
        if ret != 0 {
            bail!("NDL_DirectVideoSetArea failed: ret={ret} error={}", last_error());
        }
        Ok(())
    }

    /// Feeds one compressed access unit (Annex-B NAL units, as punktfunk's `Frame::data`
    /// already is) to the hardware decoder. PTS is derived internally as milliseconds
    /// since `load` — see module docs for why that (not the host's capture clock) is
    /// what NDL wants.
    pub fn play(&self, au: &[u8]) -> Result<()> {
        let pts_ms = self.load_instant.elapsed().as_millis() as c_longlong;
        // SAFETY: NDL_DirectVideoPlay only reads `size` bytes from `buffer` for the
        // duration of this call (it copies/consumes synchronously per the reference
        // implementation's usage) and does not retain the pointer afterward.
        let ret = unsafe { NDL_DirectVideoPlay(au.as_ptr() as *mut c_void, au.len() as c_uint, pts_ms) };
        if ret != 0 {
            bail!("NDL_DirectVideoPlay failed: ret={ret} error={}", last_error());
        }
        Ok(())
    }

    /// Sets the static HDR mastering metadata (ST.2086 + MaxCLL/MaxFALL) NDL should
    /// tag the punch-through plane with. `meta` is the host's per-session metadata
    /// (`NativeClient::next_hdr_meta`), `color` its resolved CICP colour signalling
    /// (`NativeClient::color`) — both already in the same SEI-standard units NDL's
    /// fields expect (see the struct doc comment), so this is a direct field copy,
    /// not a conversion.
    pub fn set_hdr_info(
        &self,
        meta: &punktfunk_core::quic::HdrMeta,
        color: punktfunk_core::quic::ColorInfo,
    ) -> Result<()> {
        // HdrMeta's own doc comment: "the ST.2086 RGB order is G, B, R" — already the
        // order NDL's X0/Y0, X1/Y1, X2/Y2 fields expect (same SEI convention).
        let [g, b, r] = meta.display_primaries;
        let info = NdlHdrInfo {
            display_primaries_x0: c_int::from(g[0]),
            display_primaries_y0: c_int::from(g[1]),
            display_primaries_x1: c_int::from(b[0]),
            display_primaries_y1: c_int::from(b[1]),
            display_primaries_x2: c_int::from(r[0]),
            display_primaries_y2: c_int::from(r[1]),
            white_point_x: c_int::from(meta.white_point[0]),
            white_point_y: c_int::from(meta.white_point[1]),
            max_display_mastering_luminance: meta.max_display_mastering_luminance as c_int,
            min_display_mastering_luminance: meta.min_display_mastering_luminance as c_int,
            max_content_light_level: c_int::from(meta.max_cll),
            max_pic_average_light_level: c_int::from(meta.max_fall),
            transfer_characteristics: c_int::from(color.transfer),
            color_primaries: c_int::from(color.primaries),
            matrix_coeffs: c_int::from(color.matrix),
            reserved: [0; 32],
        };
        // SAFETY: `info` is passed by value (matches the C signature exactly), no
        // pointers/aliasing involved.
        let ret = unsafe { NDL_DirectVideoSetHDRInfo(info) };
        if ret != 0 {
            bail!("NDL_DirectVideoSetHDRInfo failed: ret={ret} error={}", last_error());
        }
        Ok(())
    }

    /// Drops any buffered-but-undisplayed frames — call after a seek/loss-recovery
    /// keyframe request so stale frames don't head-of-line block the fresh one.
    pub fn flush(&self) -> Result<()> {
        // SAFETY: no arguments, no aliasing concerns.
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
        // SAFETY: no arguments; best-effort teardown, error ignored (matches
        // DestroyPlayerContext in the reference implementation — Drop can't propagate
        // a Result anyway).
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
