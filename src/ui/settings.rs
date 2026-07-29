use super::*;
use crate::store::{CodecPref, ColorRangeOverride, LogLevelOverride, Settings, VideoBackend};

/// User-requested presets: 1080p, 1440p, 4K.
pub const RESOLUTIONS: [(u32, u32, &str); 3] = [
    (1920, 1080, "1920 x 1080"),
    (2560, 1440, "2560 x 1440"),
    (3840, 2160, "3840 x 2160"),
];

/// Sent to host as exact wire refresh rate.
pub const REFRESH_RATES: [u32; 3] = [30, 60, 120];

/// Slider range: 10-200 Mbps, 5 Mbps steps.
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
/// Above this, shown as amber caution (not a hard cap).
pub const BITRATE_WARN_KBPS: u32 = 150_000;

/// Row indices for settings modal.
pub const ROW_RESOLUTION: usize = 0;
pub const ROW_FRAMERATE: usize = 1;
pub const ROW_BITRATE: usize = 2;
pub const ROW_VIDEO_BACKEND: usize = 3;
/// Directly below Video backend: only Starfish honours the VUI range flag, so the row
/// is hidden on NDL (see `color_range_row_shown`) — adjacency keeps that dependency
/// discoverable. Forces the VUI range flag sent to the decoder — see
/// `store::ColorRangeOverride`. Debug aid for the washed-out-colour investigation.
pub const ROW_COLOR_RANGE: usize = 4;
/// Below Video backend, deliberately: the AV1 option's availability depends on that
/// row's value (see `codec_options`), and adjacency is what makes the dependency
/// discoverable without explaining it in copy.
pub const ROW_CODEC: usize = 5;
/// Directly below Codec: HDR applies only to HEVC, so the row is hidden on an explicit
/// H.264 pick (see `hdr_row_shown`) — adjacency keeps that dependency discoverable.
pub const ROW_HDR: usize = 6;
pub const ROW_AUDIO: usize = 7;
/// Experimental PTS smoothing (`session::PtsPacer`) — off by default, untested on
/// real hardware; the "(experimental)" suffix in its label is the user-facing warning.
pub const ROW_VIDEO_PACING: usize = 8;
/// Not a setting — a link to `Screen::Diagnostics` (log level + stats overlay).
/// A debug aid, not something a normal user needs to find quickly.
pub const ROW_DIAGNOSTICS: usize = 9;
/// Not a setting — a link to `Screen::About`. Sits last: every other punktfunk
/// client puts the version + licences at the very bottom of Settings, and a
/// `RowKind::Action` row costs nothing extra to render.
pub const ROW_ABOUT: usize = 10;
pub const SETTINGS_ROW_COUNT: usize = 11;

/// Diagnostics modal row indices (see `diagnostics_rows`). Log level keeps index
/// 0 so its dropdown's `(Screen, row)` tile key stays stable.
pub const DIAG_ROW_LOG_LEVEL: usize = 0;
pub const DIAG_ROW_STATS_OVERLAY: usize = 1;
/// Menu-driven mirror of the Yellow-button log overlay — for remotes without one.
pub const DIAG_ROW_SHOW_LOGS: usize = 2;
/// Uploads the current session's log file to the developer (see `app::sendlogs`).
/// An action row, not a setting — Confirm opens a warning/confirmation modal first.
pub const DIAG_ROW_SEND_LOGS: usize = 3;
pub const DIAGNOSTICS_ROW_COUNT: usize = 4;

pub const COLOR_RANGE_OPTIONS: [ColorRangeOverride; 3] = [
    ColorRangeOverride::Auto,
    ColorRangeOverride::Full,
    ColorRangeOverride::Limited,
];

/// Only Starfish honours the VUI full-range flag; NDL has no equivalent field,
/// so the row is hidden there rather than shown disabled.
pub fn color_range_row_shown(settings: &Settings) -> bool {
    settings.video_backend == VideoBackend::Starfish
}

/// HDR only applies to HEVC — the host never resolves HDR for an explicit H.264
/// session, and the toggle would be a no-op. On Automatic the row stays (the host may
/// still resolve HEVC); it's hidden only when H.264 is picked explicitly. Application
/// is gated on the *negotiated* codec too — see `session::connect`.
pub fn hdr_row_shown(settings: &Settings) -> bool {
    settings.codec != CodecPref::H264
}

/// Logical `ROW_*` indices currently visible, in display order. Some rows are dropped
/// (rather than shown disabled) depending on other settings — Color range off NDL, HDR
/// on explicit H.264. Every visibility-aware helper derives from this one list.
pub fn settings_visible_logical_rows(settings: &Settings) -> Vec<usize> {
    (0..SETTINGS_ROW_COUNT)
        .filter(|&row| match row {
            ROW_HDR => hdr_row_shown(settings),
            ROW_COLOR_RANGE => color_range_row_shown(settings),
            _ => true,
        })
        .collect()
}

/// Live row count (vs. `SETTINGS_ROW_COUNT`, the maximum).
pub fn settings_row_count(settings: &Settings) -> usize {
    settings_visible_logical_rows(settings).len()
}

/// On-screen row position -> logical `ROW_*` index, skipping past any hidden rows.
pub fn settings_logical_row(settings: &Settings, display: usize) -> usize {
    settings_visible_logical_rows(settings)
        .get(display)
        .copied()
        .unwrap_or(display)
}

pub fn color_range_label(o: ColorRangeOverride) -> &'static str {
    match o {
        ColorRangeOverride::Auto => "Automatic",
        ColorRangeOverride::Full => "Full",
        ColorRangeOverride::Limited => "Limited",
    }
}

/// Cycle through options, wrapping.
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
    let mut rows = vec![
        FocusRow {
            icon: ICON_MONITOR,
            label: "Resolution".into(),
            value: resolution_label(settings.width, settings.height),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow {
            icon: ICON_SCHEDULE,
            label: "Frame rate".into(),
            value: format!("{} Hz", settings.refresh_hz),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
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
            menu: None,
        },
        FocusRow {
            icon: ICON_MEMORY,
            label: "Video backend".into(),
            value: match settings.video_backend {
                VideoBackend::Ndl => "NDL".into(),
                VideoBackend::Starfish => "Starfish".into(),
            },
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow {
            icon: ICON_PALETTE,
            label: "Color range".into(),
            value: color_range_label(settings.color_range_override).into(),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow {
            icon: ICON_MOVIE,
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
            menu: None,
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
            menu: None,
        },
        FocusRow {
            icon: ICON_SIGNAL,
            label: "Audio".into(),
            value: audio_label(settings.audio_channels),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow {
            icon: ICON_SCHEDULE,
            label: "Video pacing (experimental)".into(),
            value: if settings.video_pacing { "On".into() } else { "Off".into() },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow::action(ICON_WRENCH, "Diagnostics"),
        // The build version rides along as this row's value, so it's visible without
        // opening the screen — matching where the other clients surface it. Last row:
        // every other punktfunk client puts version + licences at the very bottom.
        FocusRow::action_with_value(ICON_INFO, "About & licenses", format!("v{VERSION}")),
    ];
    // Mirrors `settings_visible_logical_rows`: drop rather than disable when hidden.
    // Remove highest index first so an earlier removal doesn't shift a later one.
    if !hdr_row_shown(settings) {
        rows.remove(ROW_HDR);
    }
    if !color_range_row_shown(settings) {
        rows.remove(ROW_COLOR_RANGE);
    }
    rows
}

pub const LOG_LEVEL_OPTIONS: [LogLevelOverride; 4] = [
    LogLevelOverride::Debug,
    LogLevelOverride::Info,
    LogLevelOverride::Warn,
    LogLevelOverride::Error,
];

pub fn log_level_label(l: LogLevelOverride) -> &'static str {
    match l {
        LogLevelOverride::Debug => "Debug",
        LogLevelOverride::Info => "Info",
        LogLevelOverride::Warn => "Warn",
        LogLevelOverride::Error => "Error",
    }
}

/// Diagnostics' one dropdown row — options list + current index, same shape as
/// `dropdown_options`/`dropdown_current_index` but for `Screen::Diagnostics`
/// rather than a `Settings` row (there is no row-index namespace to share).
pub fn log_level_dropdown_options() -> Vec<String> {
    LOG_LEVEL_OPTIONS
        .iter()
        .map(|&l| log_level_label(l).to_string())
        .collect()
}

pub fn log_level_dropdown_current_index(level: LogLevelOverride) -> usize {
    LOG_LEVEL_OPTIONS.iter().position(|&o| o == level).unwrap_or(0)
}

/// Diagnostics modal rows: log level (dropdown), stats overlay (toggle), and
/// show logs (toggle). Order must match `DIAG_ROW_*`.
pub fn diagnostics_rows(settings: &Settings) -> Vec<FocusRow> {
    vec![
        FocusRow {
            icon: ICON_BUG,
            label: "Log level".into(),
            value: log_level_label(settings.log_level_override).into(),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow {
            icon: ICON_CHART,
            label: "Stats overlay".into(),
            value: if settings.stats_overlay {
                "On".into()
            } else {
                "Off".into()
            },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow {
            icon: ICON_VISIBILITY,
            label: "Show logs".into(),
            value: if settings.show_logs { "On".into() } else { "Off".into() },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
            menu: None,
        },
        FocusRow::action(ICON_SEND, "Send logs to developer"),
    ]
}

/// Wake settings modal rows.
pub fn wake_settings_rows(auto_send: bool) -> Vec<FocusRow> {
    vec![FocusRow {
        icon: ICON_POWER,
        label: "Wake automatically".into(),
        value: if auto_send { "On".into() } else { "Off".into() },
        kind: RowKind::Toggle,
        fraction: 0.0,
        danger: false,
        menu: None,
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

/// Supported channel counts.
pub const AUDIO_CHANNELS: [(u8, &str); 3] = [(2, "Stereo"), (6, "5.1 surround"), (8, "7.1 surround")];

fn audio_label(channels: u8) -> String {
    AUDIO_CHANNELS
        .iter()
        .find(|(c, _)| *c == channels)
        .map_or_else(|| format!("{channels} channels"), |(_, s)| (*s).to_string())
}

/// Dropdown labels for a row. Codec list depends on `video_backend`.
pub fn dropdown_options(settings: &Settings, row_index: usize) -> Vec<String> {
    match row_index {
        ROW_RESOLUTION => RESOLUTIONS.iter().map(|(w, h, _)| resolution_label(*w, *h)).collect(),
        ROW_FRAMERATE => REFRESH_RATES.iter().map(|hz| format!("{hz} Hz")).collect(),
        ROW_VIDEO_BACKEND => vec!["NDL (DirectMedia)".into(), "SMP (Starfish Media Pipeline)".into()],
        ROW_CODEC => codec_options(settings)
            .iter()
            .map(|&p| codec_label(p).to_string())
            .collect(),
        ROW_AUDIO => AUDIO_CHANNELS.iter().map(|(_, s)| (*s).to_string()).collect(),
        ROW_COLOR_RANGE => COLOR_RANGE_OPTIONS
            .iter()
            .map(|&o| color_range_label(o).to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Current dropdown index for a row's setting.
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
        ROW_COLOR_RANGE => COLOR_RANGE_OPTIONS
            .iter()
            .position(|&o| o == settings.color_range_override)
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
        ROW_COLOR_RANGE => {
            if let Some(&o) = COLOR_RANGE_OPTIONS.get(choice_index) {
                settings.color_range_override = o;
            }
        }
        _ => {}
    }
}

/// Apply left/right adjustment to a setting row. Returns true if changed.
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
        ROW_VIDEO_PACING => {
            settings.video_pacing = !settings.video_pacing;
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
        ROW_AUDIO => {
            let idx = dropdown_current_index(settings, ROW_AUDIO);
            let next = cycle_index(idx, AUDIO_CHANNELS.len(), forward);
            apply_dropdown_choice(settings, ROW_AUDIO, next);
            true
        }
        ROW_COLOR_RANGE => {
            let idx = dropdown_current_index(settings, ROW_COLOR_RANGE);
            let next = cycle_index(idx, COLOR_RANGE_OPTIONS.len(), forward);
            apply_dropdown_choice(settings, ROW_COLOR_RANGE, next);
            true
        }
        _ => false,
    }
}
