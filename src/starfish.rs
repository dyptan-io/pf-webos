//! Starfish Media Player (SMP) backend — hardware video decode via webOS's own media
//! pipeline (`StarfishMediaAPIs_C` / `libplayerAPIs_C.so`).
//!
//! Provides three capabilities NDL `DirectMedia` v2 lacks:
//! - `pauseAtDecodeTime`: Starfish paces presentation to match PTS, giving a steady
//!   decode grid when combined with frame-index PTS (see `session.rs`).
//! - `maxFrameRate` hint: sizes the decode pipeline for the actual refresh rate.
//! - `contentsType: "WEBRTC"` + `lowDelayMode`: low-latency push-stream mode.
//!
//! `libplayerAPIs_C.so` is loaded at runtime via `dlopen`; [`StarfishVideo::load`]
//! returns `Err` if the library is absent.
//!
//! # Reference
//! - `mariotaku/ss4s` modules/webos/smp — the SMP module this wraps (same API)
//! - `GuiDev1994/aurora-tv` — uses this exact path via ss4s on the same hardware
//! - `docs/NOTES.md` — documents why NDL alone is insufficient above 1080p

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::ndl::NdlCodec;

// ──────────────────────────────────────────── StarfishMediaAPIs events (0x16 etc.) ──

const EVENT_LOADCOMPLETED: c_int = 0x16;

// SDL webOS extensions from `webosbrew/SDL-webOS`. These manage an "exported window"
// surface that Starfish renders into (punch-through compositing).

#[repr(C)]
struct SdlRect {
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
}

#[link(name = "SDL2")]
extern "C" {
    fn SDL_webOSCreateExportedWindow(hint: c_int) -> *const c_char;
    fn SDL_webOSSetExportedWindow(
        window_id: *const c_char,
        src: *const SdlRect,
        dst: *const SdlRect,
    ) -> c_int;
    fn SDL_webOSDestroyExportedWindow(window_id: *const c_char);
}

// StarfishMediaAPIs_C function types — mirrored from StarfishMediaAPIs_C.h, resolved via dlsym.

type FnCreate = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FnLoad = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    Option<LoadCb>,
    *mut c_void,
) -> bool;
type FnFeed =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, usize) -> bool;
type FnPlay = unsafe extern "C" fn(*mut c_void) -> bool;
type FnPushEos = unsafe extern "C" fn(*mut c_void) -> bool;
type FnUnload = unsafe extern "C" fn(*mut c_void) -> bool;
type FnDestroy = unsafe extern "C" fn(*mut c_void);
type FnNotifyFg = unsafe extern "C" fn(*mut c_void) -> bool;
type FnSetHdrInfo = unsafe extern "C" fn(*mut c_void, *const c_char) -> bool;

type LoadCb = unsafe extern "C" fn(c_int, i64, *const c_char, *mut c_void);

unsafe fn sym<T: Sized>(lib: *mut c_void, name: &[u8]) -> Option<T> {
    let ptr = libc::dlsym(lib, name.as_ptr() as *const c_char);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: T is a function-pointer type; ptr is non-null and dlsym-verified.
        Some(std::mem::transmute_copy(&ptr))
    }
}

/// All `StarfishMediaAPIs_C` functions we call, resolved once via dlopen.
struct StarfishFns {
    create: FnCreate,
    load: FnLoad,
    feed: FnFeed,
    play: FnPlay,
    push_eos: FnPushEos,
    unload: FnUnload,
    destroy: FnDestroy,
    notify_fg: FnNotifyFg,
    set_hdr_info: FnSetHdrInfo,
}

impl StarfishFns {
    fn load_library() -> Result<Self> {
        let lib = unsafe {
            libc::dlopen(
                c"libplayerAPIs_C.so".as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_GLOBAL,
            )
        };
        if lib.is_null() {
            bail!("dlopen(libplayerAPIs_C.so) failed — Starfish/SMP not available");
        }

        macro_rules! need {
            ($sym:literal, $ty:ty) => {
                match unsafe { sym::<$ty>(lib, concat!($sym, "\0").as_bytes()) } {
                    Some(f) => f,
                    None => bail!(concat!(
                        "StarfishMediaAPIs_C.so missing symbol: ",
                        $sym
                    )),
                }
            };
        }

        Ok(Self {
            create: need!("StarfishMediaAPIs_create", FnCreate),
            load: need!("StarfishMediaAPIs_load", FnLoad),
            feed: need!("StarfishMediaAPIs_feed", FnFeed),
            play: need!("StarfishMediaAPIs_play", FnPlay),
            push_eos: need!("StarfishMediaAPIs_pushEOS", FnPushEos),
            unload: need!("StarfishMediaAPIs_unload", FnUnload),
            destroy: need!("StarfishMediaAPIs_destroy", FnDestroy),
            notify_fg: need!("StarfishMediaAPIs_notifyForeground", FnNotifyFg),
            set_hdr_info: need!("StarfishMediaAPIs_setHdrInfo", FnSetHdrInfo),
        })
    }
}

/// Callback state for `StarfishMediaAPIs_load`. Stored inside [`StarfishVideo`] so
/// the data pointer remains valid for any late-fired Starfish events.
struct LoadState {
    play_fn: FnPlay,
    api: *mut c_void,
    /// Set by the callback on LOADCOMPLETED; read by the spin-wait in `load`.
    loaded: AtomicBool,
}

// SAFETY: `api` is managed by the Starfish C library; we only store it so the
// callback thread can call `play`, and we never alias it from Rust after handoff.
unsafe impl Send for LoadState {}
unsafe impl Sync for LoadState {}

/// `StarfishMediaAPIs_load` callback. On LOADCOMPLETED, calls `play` (required by
/// the SMP API) and signals the spin-wait in [`StarfishVideo::load`].
unsafe extern "C" fn on_load_event(
    event_type: c_int,
    _num_value: i64,
    _str_value: *const c_char,
    data: *mut c_void,
) {
    if data.is_null() {
        return;
    }
    let state = &*(data as *const LoadState);
    if event_type == EVENT_LOADCOMPLETED {
        (state.play_fn)(state.api);
        state.loaded.store(true, Ordering::Release);
    }
}

/// One active Starfish SMP video decode+present session.
pub struct StarfishVideo {
    fns: StarfishFns,
    api: *mut c_void,
    window_id_cstr: CString,
    /// Kept alive so the `data` pointer given to `StarfishMediaAPIs_load` remains
    /// valid for the session lifetime.
    _load_state: Box<LoadState>,
}

// SAFETY: owned exclusively by the video-pump thread; raw pointers managed by Starfish.
unsafe impl Send for StarfishVideo {}

impl StarfishVideo {
    /// Load Starfish for a video stream. Creates an exported SDL window for
    /// punch-through rendering, builds the JSON payload with `pauseAtDecodeTime`,
    /// `maxFrameRate`, and `WEBRTC` low-latency mode, and waits for LOADCOMPLETED.
    ///
    /// Returns `Err` if `libplayerAPIs_C.so` is absent (caller falls back to NDL).
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        app_id: &str,
        width: i32,
        height: i32,
        fps: u32,
        codec: NdlCodec,
        display_w: i32,
        display_h: i32,
        log: &mut std::fs::File,
    ) -> Result<Self> {
        use std::io::Write as _;

        let fns = StarfishFns::load_library()?;

        let api = unsafe { (fns.create)(std::ptr::null()) };
        if api.is_null() {
            bail!("StarfishMediaAPIs_create returned null");
        }
        unsafe { (fns.notify_fg)(api) };

        // Create the exported window for video punch-through.
        let raw_window_id = unsafe { SDL_webOSCreateExportedWindow(0) };
        if raw_window_id.is_null() {
            unsafe { (fns.destroy)(api) };
            bail!("SDL_webOSCreateExportedWindow returned null");
        }

        let window_id_cstr = unsafe { CStr::from_ptr(raw_window_id) }.to_owned();
        let window_id_str = unsafe { CStr::from_ptr(raw_window_id) }
            .to_str()
            .unwrap_or("");

        let codec_str = match codec {
            NdlCodec::H264 => "H264",
            NdlCodec::H265 => "H265",
            NdlCodec::Av1 => "AV1",
        };

        // Load payload — mirrors ss4s MakeLoadPayload for the webOS 5 SMP module.
        let payload = serde_json::json!({
            "args": [{
                "mediaTransportType": "BUFFERSTREAM",
                "option": {
                    "appId": app_id,
                    "externalStreamingInfo": {
                        "contents": {
                            "codec": {"video": codec_str},
                            "esInfo": {
                                "pauseAtDecodeTime": true,
                                "ptsToDecode": 0,
                                "seperatedPTS": true
                            },
                            "format": "RAW",
                            "provider": "Chrome"
                        },
                        "streamQualityInfo": true,
                        "audioSync": true,
                        "streamQualityInfoCorruptedFrame": true,
                        "streamQualityInfoNonFlushable": true,
                        "restartStreaming": false,
                        "bufferingCtrInfo": {
                            "bufferMaxLevel": 0,
                            "bufferMinLevel": 0,
                            "preBufferByte": 0,
                            "qBufferLevelAudio": 0,
                            "qBufferLevelVideo": 0,
                            "srcBufferLevelAudio": {"minimum": 1, "maximum": 32768},
                            "srcBufferLevelVideo": {"minimum": 1, "maximum": 1048576}
                        }
                    },
                    "transmission": {"contentsType": "WEBRTC"},
                    "needAudio": false,
                    "queryPosition": false,
                    "lowDelayMode": true,
                    "adaptiveStreaming": {
                        "audioOnly": false,
                        "maxWidth": width,
                        "maxHeight": height,
                        "maxFrameRate": f64::from(fps)
                    },
                    "windowId": window_id_str
                }
            }]
        })
        .to_string();

        writeln!(
            log,
            "Starfish load payload: {}",
            &payload[..payload.len().min(512)]
        )?;

        let payload_cstr = CString::new(payload)?;

        let load_state = Box::new(LoadState {
            play_fn: fns.play,
            api,
            loaded: AtomicBool::new(false),
        });
        let state_ptr = &*load_state as *const LoadState as *mut c_void;

        // SAFETY: `payload_cstr` and `state_ptr` outlive this call; `state_ptr`
        // remains valid for the session (owned by `load_state` in `Self`).
        let load_ok =
            unsafe { (fns.load)(api, payload_cstr.as_ptr(), Some(on_load_event), state_ptr) };
        if !load_ok {
            unsafe {
                (fns.destroy)(api);
                SDL_webOSDestroyExportedWindow(raw_window_id);
            }
            bail!("StarfishMediaAPIs_load returned false");
        }

        // Spin-wait for LOADCOMPLETED (cap at 5s).
        let deadline = Instant::now() + Duration::from_secs(5);
        while !load_state.loaded.load(Ordering::Acquire) {
            if Instant::now() > deadline {
                unsafe {
                    (fns.destroy)(api);
                    SDL_webOSDestroyExportedWindow(raw_window_id);
                }
                bail!("StarfishMediaAPIs_load timeout — LOADCOMPLETED never arrived");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Bind source (stream rect) and destination (full display) punch-through area
        // only after load completes — matches ss4s's `StarfishResourcePostLoad` timing,
        // rather than binding it before the pipeline exists.
        let src = SdlRect { x: 0, y: 0, w: width, h: height };
        let dst = SdlRect { x: 0, y: 0, w: display_w, h: display_h };
        unsafe { SDL_webOSSetExportedWindow(raw_window_id, &src, &dst) };

        Ok(Self {
            fns,
            api,
            window_id_cstr,
            _load_state: load_state,
        })
    }

    /// Feed one access unit. `pts_ns` is nanoseconds (`frame.pts_ns` from the host).
    pub fn play(&self, au: &[u8], pts_ns: u64) -> Result<()> {
        let payload = format!(
            r#"{{"bufferAddr":"{:p}","bufferSize":{},"pts":{},"esData":1}}"#,
            au.as_ptr(),
            au.len(),
            pts_ns
        );
        let payload_cstr = CString::new(payload)?;
        let mut result = [0u8; 256];

        // SAFETY: all args valid for the duration of this call.
        unsafe {
            (self.fns.feed)(
                self.api,
                payload_cstr.as_ptr(),
                result.as_mut_ptr() as *mut c_char,
                result.len(),
            )
        };

        let result_str = std::str::from_utf8(&result)
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim();
        // Feed's result is JSON, e.g. `{"returnValue":"Ok"}` — not a bare string.
        let return_value = serde_json::from_str::<serde_json::Value>(result_str)
            .ok()
            .and_then(|v| v.get("returnValue")?.as_str().map(str::to_owned));
        match return_value.as_deref() {
            Some("Ok") => Ok(()),
            Some("BufferFull") => bail!("StarfishMediaAPIs_feed: BufferFull"),
            _ => bail!("StarfishMediaAPIs_feed failed: {result_str}"),
        }
    }

    /// Flush stale pre-loss frames.
    ///
    /// Starfish BUFFERSTREAM pipelines reject all feed calls after `pushEOS`, so
    /// EOS must not be used here. With `bufferMinLevel: 0` the internal buffer is
    /// always near-empty, so stale frames are already displayed by the time loss
    /// is detected. The IDR that arrives after hold recovery resets decoder state
    /// natively — no explicit flush is needed.
    pub fn flush(&self) -> Result<()> {
        Ok(())
    }

    /// Apply HDR10 mastering metadata (ss4s `smp_video.c::SetHDRInfo` JSON structure).
    /// Forwards the stream's colorimetry (and, for HDR, its mastering metadata)
    /// to SMP. `meta: None` = an SDR stream: `hdrType` is "none" and only the
    /// `vui` colour triplet is sent — see `ndl.rs::set_color_info` for why SDR
    /// colorimetry must be forwarded at all (4K-decodes-as-BT.2020 washout).
    pub fn set_color_info(
        &self,
        meta: Option<&punktfunk_core::quic::HdrMeta>,
        color: punktfunk_core::quic::ColorInfo,
    ) -> Result<()> {
        // The stream's own range flag, not a hardcoded value.
        let vui = serde_json::json!({
            "transferCharacteristics": color.transfer,
            "colorPrimaries": color.primaries,
            "matrixCoeffs": color.matrix,
            "videoFullRangeFlag": color.full_range != 0
        });
        let payload = match meta {
            Some(meta) => {
                // G/B/R order per ST.2086 convention (same as ndl.rs).
                let [g, b, r] = meta.display_primaries;
                serde_json::json!({
                    "hdrType": "HDR10",
                    "sei": {
                        "displayPrimariesX0": g[0],
                        "displayPrimariesY0": g[1],
                        "displayPrimariesX1": b[0],
                        "displayPrimariesY1": b[1],
                        "displayPrimariesX2": r[0],
                        "displayPrimariesY2": r[1],
                        "whitePointX": meta.white_point[0],
                        "whitePointY": meta.white_point[1],
                        "minDisplayMasteringLuminance": meta.min_display_mastering_luminance,
                        "maxDisplayMasteringLuminance": meta.max_display_mastering_luminance,
                        "maxContentLightLevel": meta.max_cll,
                        "maxPicAverageLightLevel": meta.max_fall
                    },
                    "vui": vui
                })
            }
            None => serde_json::json!({
                "hdrType": "none",
                "vui": vui
            }),
        }
        .to_string();

        let payload_cstr = CString::new(payload)?;
        // SAFETY: all args valid for the duration of this call.
        let ok = unsafe { (self.fns.set_hdr_info)(self.api, payload_cstr.as_ptr()) };
        if !ok {
            bail!("StarfishMediaAPIs_setHdrInfo failed");
        }
        Ok(())
    }
}

impl Drop for StarfishVideo {
    fn drop(&mut self) {
        // SAFETY: `api` and `window_id_cstr` are valid for the lifetime of `Self`.
        unsafe {
            (self.fns.push_eos)(self.api);
            (self.fns.unload)(self.api);
            (self.fns.destroy)(self.api);
            SDL_webOSDestroyExportedWindow(self.window_id_cstr.as_ptr());
        }
    }
}
