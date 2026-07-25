//! Settings-screen data: presets, row indices, and the row list/adjust logic.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.
use super::*;
use crate::store::{CodecPref, Settings, VideoBackend};

/// Resolution presets — the three the user asked for, matching `pf-console-ui`'s
/// existing 1080p/1440p/4K entries (a subset of its full list; no 720p/800p here).
pub const RESOLUTIONS: [(u32, u32, &str); 3] = [
    (1920, 1080, "1920 x 1080"),
    (2560, 1440, "2560 x 1440"),
    (3840, 2160, "3840 x 2160"),
];

/// Framerate presets — sent to the host as the exact wire refresh rate.
pub const REFRESH_RATES: [u32; 3] = [30, 60, 120];

/// Bitrate slider range/step, in kbps — the user's explicit ask ("10-200 Mbps max").
pub const BITRATE_MIN_KBPS: u32 = 10_000;
pub const BITRATE_MAX_KBPS: u32 = 200_000;
pub const BITRATE_STEP_KBPS: u32 = 5_000;
/// Sentinel one notch below `BITRATE_MIN_KBPS` on the slider: `punktfunk_core::client::NativeClient`
/// arms its own client-side AIMD bitrate controller (`punktfunk_core::abr`) precisely when it's
/// asked to connect with `bitrate_kbps == 0` — it reacts to unrecoverable frames, heavy loss,
/// one-way-delay rise, and (via `session.rs`'s `report_decode_us` call) decode latency, backing off
/// or climbing every ~750ms. A fixed Mbps number, however carefully picked, never adapts to a link
/// that degrades mid-session — this does.
pub const BITRATE_AUTOMATIC: u32 = 0;
/// Above this, stability drops off on typical Wi-Fi — shown as an amber
/// caution, matching the reference's settings pane (not a hard cap, the
/// slider still allows up to `BITRATE_MAX_KBPS`).
pub const BITRATE_WARN_KBPS: u32 = 150_000;

/// Settings-modal row indices — shared by `settings_rows`, `adjust_setting`, and
/// `app.rs`'s event handling so the mapping only lives in one place.
pub const ROW_RESOLUTION: usize = 0;
pub const ROW_FRAMERATE: usize = 1;
pub const ROW_BITRATE: usize = 2;
pub const ROW_HDR: usize = 3;
pub const ROW_VIDEO_BACKEND: usize = 4;
/// Directly below Video backend, deliberately: the AV1 option's availability depends
/// on that row's value (see `codec_options`), and adjacency is what makes the
/// dependency discoverable without explaining it in copy.
pub const ROW_CODEC: usize = 5;
pub const ROW_STATS_OVERLAY: usize = 6;
pub const ROW_AUDIO: usize = 7;
/// Not a setting — a link to `Screen::About`. It lives in this list (rather than as
/// separate chrome) because that is where every other punktfunk client puts the
/// version + licences, and a `RowKind::Action` row costs nothing extra to render.
pub const ROW_ABOUT: usize = 8;
pub const SETTINGS_ROW_COUNT: usize = 9;

/// Cycles `current` to the next/previous value in a preset slice, wrapping.
pub fn cycle<T: Copy + PartialEq>(options: &[T], current: T, forward: bool) -> T {
    let idx = options.iter().position(|&o| o == current).unwrap_or(0);
    let len = options.len();
    let next = if forward {
        (idx + 1) % len
    } else {
        (idx + len - 1) % len
    };
    options[next]
}

pub fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

pub fn resolution_label(width: u32, height: u32) -> String {
    RESOLUTIONS
        .iter()
        .find(|(w, h, _)| *w == width && *h == height)
        .map_or_else(|| format!("{width}x{height}"), |(_, _, s)| s.to_string())
}

pub fn settings_rows(settings: &Settings) -> Vec<FocusRow> {
    let bitrate_frac = if settings.bitrate_kbps == BITRATE_AUTOMATIC {
        0.0
    } else {
        (settings.bitrate_kbps.saturating_sub(BITRATE_MIN_KBPS)) as f32 / (BITRATE_MAX_KBPS - BITRATE_MIN_KBPS) as f32
    };
    vec![
        FocusRow {
            icon: ICON_MONITOR,
            label: "Resolution".into(),
            value: resolution_label(settings.width, settings.height),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
        },
        FocusRow {
            icon: ICON_SCHEDULE,
            label: "Frame rate".into(),
            value: format!("{} Hz", settings.refresh_hz),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
        },
        FocusRow {
            icon: ICON_SIGNAL,
            label: "Bitrate".into(),
            value: if settings.bitrate_kbps == BITRATE_AUTOMATIC {
                "Automatic".into()
            } else {
                format!("{} Mbps", settings.bitrate_kbps / 1000)
            },
            kind: RowKind::Slider,
            fraction: bitrate_frac,
            danger: false,
        },
        FocusRow {
            icon: ICON_SUN,
            label: "HDR".into(),
            value: if settings.hdr_enabled {
                "On".into()
            } else {
                "Off".into()
            },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
        },
        FocusRow {
            icon: ICON_TV,
            label: "Video backend".into(),
            value: match settings.video_backend {
                VideoBackend::Ndl => "NDL".into(),
                VideoBackend::Starfish => "Starfish".into(),
            },
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
        },
        FocusRow {
            icon: ICON_MONITOR,
            label: "Codec".into(),
            // A persisted choice that is no longer offered (AV1 after Starfish proved it
            // won't load this run) says so, rather than displaying a codec the session
            // will silently not use — `session::connect` clamps it back to Automatic.
            value: if codec_options(settings).contains(&settings.codec) {
                codec_label(settings.codec).into()
            } else {
                format!("{} (unavailable)", codec_label(settings.codec))
            },
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
        },
        FocusRow {
            icon: ICON_SUN,
            label: "Stats overlay".into(),
            value: if settings.stats_overlay {
                "On".into()
            } else {
                "Off".into()
            },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
        },
        FocusRow {
            icon: ICON_SIGNAL,
            label: "Audio".into(),
            value: audio_label(settings.audio_channels),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
        },
        // The build version rides along as this row's value, so it's visible without
        // opening the screen — matching where the other clients surface it.
        FocusRow::action_with_value(ICON_INFO, "About & licenses", format!("v{VERSION}")),
    ]
}

/// The Wake modal's one row — the "Always send automatically" toggle (see
/// `app::WakeState`) — as a single-element `FocusRow` list, so it draws and
/// zoom-animates through the exact same `draw_focus_rows`/
/// `render_focus_row_tile` machinery as the settings modal. The actual
/// "Wake"/"Cancel" actions are a `draw_confirm_buttons` row below this one
/// (see `app.rs`'s `render_wake`), not rows here — mirroring the
/// Forget-host confirmation's shell/buttons split.
pub fn wake_rows(auto_send: bool) -> Vec<FocusRow> {
    vec![FocusRow {
        icon: ICON_SETTINGS,
        label: "Wake automatically in future".into(),
        value: if auto_send { "On".into() } else { "Off".into() },
        kind: RowKind::Toggle,
        fraction: 0.0,
        danger: false,
    }]
}

/// The codec choices offered right now — which is why this is a function of the live
/// `Settings`, not a const: AV1 appears only when the Starfish backend is selected
/// (NDL's impl can't decode it — docs/NOTES.md) *and* this TV's platform decoder
/// actually declares AV1 (`device::supports_av1`, probed once per run). Everything
/// downstream (dropdown, Left/Right cycling, current-index lookup) derives from this
/// one list, so an unavailable AV1 simply doesn't exist as an option anywhere.
pub fn codec_options(settings: &Settings) -> Vec<CodecPref> {
    let mut options = vec![CodecPref::Auto, CodecPref::H264, CodecPref::Hevc];
    // Four conditions, and AV1 has never satisfied the last one on real hardware — see
    // `store::dev_override_enable_av1`. The other three stay because each rules out a
    // distinct way of handing a decoder something it can't present.
    if crate::store::dev_override_enable_av1()
        && settings.video_backend == VideoBackend::Starfish
        && crate::device::supports_av1()
        && !crate::starfish::proven_unavailable()
    {
        options.push(CodecPref::Av1);
    }
    options
}

pub fn codec_label(pref: CodecPref) -> &'static str {
    match pref {
        CodecPref::Auto => "Automatic",
        CodecPref::H264 => "H.264",
        CodecPref::Hevc => "HEVC",
        CodecPref::Av1 => "AV1",
    }
}

/// Channel counts punktfunk negotiates, and how they read on screen.
pub const AUDIO_CHANNELS: [(u8, &str); 3] = [(2, "Stereo"), (6, "5.1 surround"), (8, "7.1 surround")];

fn audio_label(channels: u8) -> String {
    AUDIO_CHANNELS
        .iter()
        .find(|(c, _)| *c == channels)
        .map_or_else(|| format!("{channels} channels"), |(_, s)| (*s).to_string())
}

/// The option labels for a dropdown row (`Resolution`/`Frame rate`/`Video backend`/
/// `Codec`/`Audio`). Takes the live `Settings` because the Codec row's list is
/// state-dependent (see `codec_options`).
pub fn dropdown_options(settings: &Settings, row_index: usize) -> Vec<String> {
    match row_index {
        ROW_RESOLUTION => RESOLUTIONS.iter().map(|(w, h, _)| resolution_label(*w, *h)).collect(),
        ROW_FRAMERATE => REFRESH_RATES.iter().map(|hz| format!("{hz} Hz")).collect(),
        ROW_VIDEO_BACKEND => vec!["NDL".into(), "Starfish".into()],
        ROW_CODEC => codec_options(settings).iter().map(|&p| codec_label(p).to_string()).collect(),
        ROW_AUDIO => AUDIO_CHANNELS.iter().map(|(_, s)| (*s).to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Which option index in `dropdown_options(row_index)` matches the current setting.
pub fn dropdown_current_index(settings: &Settings, row_index: usize) -> usize {
    match row_index {
        ROW_RESOLUTION => RESOLUTIONS
            .iter()
            .position(|(w, h, _)| *w == settings.width && *h == settings.height)
            .unwrap_or(0),
        ROW_FRAMERATE => REFRESH_RATES
            .iter()
            .position(|hz| *hz == settings.refresh_hz)
            .unwrap_or(0),
        ROW_VIDEO_BACKEND => match settings.video_backend {
            VideoBackend::Ndl => 0,
            VideoBackend::Starfish => 1,
        },
        ROW_CODEC => codec_options(settings)
            .iter()
            .position(|&p| p == settings.codec)
            .unwrap_or(0),
        ROW_AUDIO => AUDIO_CHANNELS
            .iter()
            .position(|(c, _)| *c == settings.audio_channels)
            .unwrap_or(0),
        _ => 0,
    }
}

pub fn apply_dropdown_choice(settings: &mut Settings, row_index: usize, choice_index: usize) {
    match row_index {
        ROW_RESOLUTION => {
            if let Some((w, h, _)) = RESOLUTIONS.get(choice_index) {
                settings.width = *w;
                settings.height = *h;
            }
        }
        ROW_FRAMERATE => {
            if let Some(hz) = REFRESH_RATES.get(choice_index) {
                settings.refresh_hz = *hz;
            }
        }
        ROW_VIDEO_BACKEND => {
            settings.video_backend = match choice_index {
                1 => VideoBackend::Starfish,
                _ => VideoBackend::Ndl,
            };
            // AV1 only exists as a choice under Starfish (see `codec_options`) —
            // switching away must take the stranded preference with it, or it would
            // silently ride along invisible (no row shows it, connect would clamp it).
            if settings.video_backend != VideoBackend::Starfish && settings.codec == CodecPref::Av1 {
                settings.codec = CodecPref::Auto;
            }
        }
        ROW_CODEC => {
            if let Some(&pref) = codec_options(settings).get(choice_index) {
                settings.codec = pref;
            }
        }
        ROW_AUDIO => {
            if let Some((channels, _)) = AUDIO_CHANNELS.get(choice_index) {
                settings.audio_channels = *channels;
            }
        }
        _ => {}
    }
}

/// Applies a left/right adjustment to `settings` for the given settings-row index.
/// Returns `true` if it changed.
pub fn adjust_setting(settings: &mut Settings, row_index: usize, forward: bool) -> bool {
    match row_index {
        ROW_RESOLUTION => {
            let idx = dropdown_current_index(settings, row_index);
            let next = cycle_index(idx, RESOLUTIONS.len(), forward);
            apply_dropdown_choice(settings, row_index, next);
            true
        }
        ROW_FRAMERATE => {
            settings.refresh_hz = cycle(&REFRESH_RATES, settings.refresh_hz, forward);
            true
        }
        ROW_BITRATE => {
            if settings.bitrate_kbps == BITRATE_AUTOMATIC {
                if forward {
                    settings.bitrate_kbps = BITRATE_MIN_KBPS;
                }
                // Already at the floor going backward from Automatic — nothing below it.
            } else if !forward && settings.bitrate_kbps == BITRATE_MIN_KBPS {
                settings.bitrate_kbps = BITRATE_AUTOMATIC;
            } else {
                let delta = i64::from(BITRATE_STEP_KBPS) * if forward { 1 } else { -1 };
                let next = (i64::from(settings.bitrate_kbps) + delta)
                    .clamp(i64::from(BITRATE_MIN_KBPS), i64::from(BITRATE_MAX_KBPS));
                settings.bitrate_kbps = next as u32;
            }
            true
        }
        ROW_HDR => {
            settings.hdr_enabled = !settings.hdr_enabled;
            true
        }
        ROW_VIDEO_BACKEND => {
            let idx = dropdown_current_index(settings, ROW_VIDEO_BACKEND);
            let next = cycle_index(idx, 2, forward);
            apply_dropdown_choice(settings, ROW_VIDEO_BACKEND, next);
            true
        }
        ROW_CODEC => {
            let idx = dropdown_current_index(settings, ROW_CODEC);
            let next = cycle_index(idx, codec_options(settings).len(), forward);
            apply_dropdown_choice(settings, ROW_CODEC, next);
            true
        }
        ROW_STATS_OVERLAY => {
            settings.stats_overlay = !settings.stats_overlay;
            true
        }
        ROW_AUDIO => {
            let idx = dropdown_current_index(settings, ROW_AUDIO);
            let next = cycle_index(idx, AUDIO_CHANNELS.len(), forward);
            apply_dropdown_choice(settings, ROW_AUDIO, next);
            true
        }
        _ => false,
    }
}
