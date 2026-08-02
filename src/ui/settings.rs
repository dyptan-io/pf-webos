use super::*;
use crate::services::store::{CodecPref, GamepadType, LogLevelOverride, Settings};

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
/// Above this, the Bitrate row shows a dull-orange caution caption (not a hard cap).
pub const BITRATE_WARN_KBPS: u32 = 150_000;

/// Card space above the row list: title, divider, and their padding.
pub const SETTINGS_CHROME_TOP: u32 = 120;

/// Card space below the row list: just enough to clear the card's rounded corner, so the
/// list runs to the card's edge and the bottom fade dissolves into it.
///
/// Anything more shows as a band of flat card background under the fade — the fade already
/// *is* the bottom edge, so padding beneath it reads as dead space rather than breathing room.
pub const SETTINGS_CHROME_BOTTOM: u32 = 16;

/// Minimum gap between the settings card and the screen edges, top and bottom combined.
///
/// Trimmed from 160 when the second peek strip arrived: two 44px peeks cost a whole visible
/// row out of a 1080p budget, and the card had more inset to spare than the list had rows.
pub const SETTINGS_EDGE_MARGIN: u32 = 120;

/// How much of the adjacent row stays visible past each edge of the viewport while the list
/// overflows — the strip an edge fade dissolves. Applied to the top and bottom alike.
///
/// Load-bearing, not decoration: a viewport edge landing exactly on a row boundary has
/// nothing but card background in its outermost pixels (unfocused rows draw no fill of their
/// own), so a fade there blends the card colour into the card colour and is *mathematically
/// invisible*. Both cuts have to land mid-row for either fade to read at all — which is also
/// why the rendered offset is biased by one peek (see `App::sync_modal_scroll`) instead of
/// sitting on the row grid.
///
/// Independent of [`SCROLL_FADE_H`], which is taller: this is how much of the next row is
/// *exposed*, while that is how far the fade reaches back over what is already visible. Deep
/// enough to expose a row's icon and label, which sit in the middle third of its height — a
/// shallower peek shows only the row's internal padding, i.e. nothing to dissolve.
pub const SETTINGS_PEEK: u32 = 44;

/// Pixels between the tops of consecutive settings rows.
pub const fn settings_row_stride() -> u32 {
    SETTINGS_ROW_H + SETTINGS_ROW_GAP as u32
}

/// Row indices for settings modal.
pub const ROW_RESOLUTION: usize = 0;
pub const ROW_FRAMERATE: usize = 1;
pub const ROW_BITRATE: usize = 2;
pub const ROW_CODEC: usize = 3;
/// Directly below Codec: HDR applies only to HEVC, so the row is hidden on an explicit
/// H.264 pick (see `hdr_row_shown`) — adjacency keeps that dependency discoverable.
pub const ROW_HDR: usize = 4;
pub const ROW_AUDIO: usize = 5;
/// Which controller the host presents to the game — see `store::GamepadType`. Last of the
/// real settings: it's the only input-side one, and picking `DualSense` is what turns on
/// adaptive triggers (`crate::platform::webos::dualsense`).
pub const ROW_GAMEPAD: usize = 6;
/// Directly below Controller — the other input-side setting. See
/// `store::Settings::cursor_capture`.
pub const ROW_CURSOR_CAPTURE: usize = 7;
/// Not a setting — a link to `Screen::Experimental` (unstable toggles, currently the
/// frame pacer). Grouped off the main list so an untested option isn't one keystroke away.
pub const ROW_EXPERIMENTAL: usize = 8;
/// Not a setting — a link to `Screen::Diagnostics` (log level + stats overlay).
/// A debug aid, not something a normal user needs to find quickly.
pub const ROW_DIAGNOSTICS: usize = 9;
/// Not a setting — a link to `Screen::About`. Sits last: every other punktfunk
/// client puts the version + licences at the very bottom of Settings, and a
/// `RowKind::Action` row costs nothing extra to render.
pub const ROW_ABOUT: usize = 10;
pub const SETTINGS_ROW_COUNT: usize = 11;

/// Experimental modal row indices (see `experimental_rows`).
pub const EXP_ROW_FRAME_PACER: usize = 0;
/// Only present on rooted TVs (see `experimental_rows`), so it's the last row when shown.
pub const EXP_ROW_GAME_MODE: usize = 1;

/// Live experimental-row count without building the rows — the Game mode row is only offered on
/// a rooted TV (see `experimental_rows`), so the screen is one row shorter otherwise. Used by the
/// card/hit-test sizing paths that need the count but not the `FocusRow` allocations.
pub fn experimental_row_count(rooted: bool) -> usize {
    1 + usize::from(rooted)
}

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

/// HDR only applies to HEVC — the host never resolves HDR for an explicit H.264
/// session, and the toggle would be a no-op. On Automatic the row stays (the host may
/// still resolve HEVC); it's hidden only when H.264 is picked explicitly. Application
/// is gated on the *negotiated* codec too — see `session::connect`.
pub fn hdr_row_shown(settings: &Settings) -> bool {
    settings.codec != CodecPref::H264
}

/// Logical `ROW_*` indices currently visible, in display order. The HDR row is dropped
/// (rather than shown disabled) on an explicit H.264 pick. Every visibility-aware helper
/// derives from this one list.
pub fn settings_visible_logical_rows(settings: &Settings) -> Vec<usize> {
    (0..SETTINGS_ROW_COUNT)
        .filter(|&row| match row {
            ROW_HDR => hdr_row_shown(settings),
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
            subtext: None,
        },
        FocusRow {
            icon: ICON_SCHEDULE,
            label: "Frame rate".into(),
            value: format!("{} Hz", settings.refresh_hz),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
            subtext: None,
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
            subtext: (settings.bitrate_kbps > BITRATE_WARN_KBPS)
                .then(|| RowSubtext::caution("May be unstable on Wi-Fi — try Ethernet")),
        },
        FocusRow {
            icon: ICON_MOVIE,
            label: "Codec".into(),
            value: codec_label(settings.codec).into(),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
            subtext: None,
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
            subtext: None,
        },
        FocusRow {
            icon: ICON_SIGNAL,
            label: "Audio".into(),
            value: audio_label(settings.audio_channels),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
            subtext: None,
        },
        FocusRow {
            icon: ICON_GAMEPAD,
            label: "Controller".into(),
            value: gamepad_label(settings.gamepad_type).into(),
            kind: RowKind::Dropdown,
            fraction: 0.0,
            danger: false,
            menu: None,
            subtext: None,
        },
        FocusRow {
            icon: ICON_MOUSE,
            label: "Cursor capture".into(),
            value: if settings.cursor_capture {
                "On".into()
            } else {
                "Off".into()
            },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
            menu: None,
            subtext: None,
        },
        FocusRow::action(ICON_BUG, "Experimental"),
        FocusRow::action(ICON_WRENCH, "Diagnostics"),
        // The build version rides along as this row's value, so it's visible without
        // opening the screen — matching where the other clients surface it. Last row:
        // every other punktfunk client puts version + licences at the very bottom.
        FocusRow::action_with_value(ICON_INFO, "About & licenses", format!("v{VERSION}")),
    ];
    // Mirrors `settings_visible_logical_rows`: drop rather than disable when hidden.
    if !hdr_row_shown(settings) {
        rows.remove(ROW_HDR);
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
            subtext: None,
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
            subtext: None,
        },
        FocusRow {
            icon: ICON_VISIBILITY,
            label: "Show logs".into(),
            value: if settings.show_logs { "On".into() } else { "Off".into() },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
            menu: None,
            subtext: None,
        },
        FocusRow::action(ICON_SEND, "Send logs to developer"),
    ]
}

/// Experimental modal rows: the frame pacer toggle (`session::PtsPacer`). Off by default,
/// untested on hardware, live-toggleable mid-stream with the Blue button. Order must match
/// `EXP_ROW_*`.
pub fn experimental_rows(settings: &Settings, rooted: bool) -> Vec<FocusRow> {
    let mut rows = vec![FocusRow {
        icon: ICON_SCHEDULE,
        label: "Frame pacer".into(),
        value: if settings.video_pacing {
            "On".into()
        } else {
            "Off".into()
        },
        kind: RowKind::Toggle,
        fraction: 0.0,
        danger: false,
        menu: None,
        subtext: Some(RowSubtext::hint("Toggles live with the Blue button")),
    }];
    // Driving the TV's Game picture/sound modes needs the Homebrew Channel's root helper — the
    // public bus is denied `settingsservice` outright (see `platform::webos::game_mode`). So the
    // row only exists on a rooted set, where it's known to work.
    if rooted {
        rows.push(FocusRow {
            icon: ICON_GAMEPAD,
            label: "Game mode".into(),
            value: if settings.game_mode { "On".into() } else { "Off".into() },
            kind: RowKind::Toggle,
            fraction: 0.0,
            danger: false,
            menu: None,
            subtext: Some(RowSubtext::hint("Your TV is rooted, you can use ALLM")),
        });
    }
    rows
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
        subtext: None,
    }]
}

/// The codec choices offered. NDL decodes H.264/HEVC only, so the list is fixed.
pub fn codec_options() -> Vec<CodecPref> {
    vec![CodecPref::Auto, CodecPref::H264, CodecPref::Hevc]
}

pub fn codec_label(pref: CodecPref) -> &'static str {
    match pref {
        CodecPref::Auto => "Automatic",
        CodecPref::H264 => "H.264",
        CodecPref::Hevc => "HEVC",
    }
}

/// Controller types offered, in display order. `Automatic` first (the default, and what an
/// existing install already has); the rest are ordered by how likely a TV user is to own one.
pub const GAMEPAD_TYPES: [GamepadType; 7] = [
    GamepadType::Auto,
    GamepadType::DualSense,
    GamepadType::DualSenseEdge,
    GamepadType::DualShock4,
    GamepadType::Xbox360,
    GamepadType::XboxOne,
    GamepadType::SwitchPro,
];

pub fn gamepad_label(t: GamepadType) -> &'static str {
    match t {
        GamepadType::Auto => "Automatic",
        GamepadType::Xbox360 => "Xbox 360",
        GamepadType::XboxOne => "Xbox One",
        GamepadType::DualShock4 => "DualShock 4",
        GamepadType::DualSense => "DualSense",
        GamepadType::DualSenseEdge => "DualSense Edge",
        GamepadType::SwitchPro => "Switch Pro",
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

/// Dropdown labels for a row.
pub fn dropdown_options(settings: &Settings, row_index: usize) -> Vec<String> {
    let _ = settings;
    match row_index {
        ROW_RESOLUTION => RESOLUTIONS.iter().map(|(w, h, _)| resolution_label(*w, *h)).collect(),
        ROW_FRAMERATE => REFRESH_RATES.iter().map(|hz| format!("{hz} Hz")).collect(),
        ROW_CODEC => codec_options().iter().map(|&p| codec_label(p).to_string()).collect(),
        ROW_AUDIO => AUDIO_CHANNELS.iter().map(|(_, s)| (*s).to_string()).collect(),
        ROW_GAMEPAD => GAMEPAD_TYPES.iter().map(|&t| gamepad_label(t).to_string()).collect(),
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
        ROW_CODEC => codec_options().iter().position(|&p| p == settings.codec).unwrap_or(0),
        ROW_AUDIO => AUDIO_CHANNELS
            .iter()
            .position(|(c, _)| *c == settings.audio_channels)
            .unwrap_or(0),
        ROW_GAMEPAD => GAMEPAD_TYPES
            .iter()
            .position(|&t| t == settings.gamepad_type)
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
        ROW_CODEC => {
            if let Some(&pref) = codec_options().get(choice_index) {
                settings.codec = pref;
            }
        }
        ROW_AUDIO => {
            if let Some((channels, _)) = AUDIO_CHANNELS.get(choice_index) {
                settings.audio_channels = *channels;
            }
        }
        ROW_GAMEPAD => {
            if let Some(&t) = GAMEPAD_TYPES.get(choice_index) {
                settings.gamepad_type = t;
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
        ROW_CURSOR_CAPTURE => {
            settings.cursor_capture = !settings.cursor_capture;
            true
        }
        ROW_CODEC => {
            let idx = dropdown_current_index(settings, ROW_CODEC);
            let next = cycle_index(idx, codec_options().len(), forward);
            apply_dropdown_choice(settings, ROW_CODEC, next);
            true
        }
        ROW_AUDIO => {
            let idx = dropdown_current_index(settings, ROW_AUDIO);
            let next = cycle_index(idx, AUDIO_CHANNELS.len(), forward);
            apply_dropdown_choice(settings, ROW_AUDIO, next);
            true
        }
        ROW_GAMEPAD => {
            let idx = dropdown_current_index(settings, ROW_GAMEPAD);
            let next = cycle_index(idx, GAMEPAD_TYPES.len(), forward);
            apply_dropdown_choice(settings, ROW_GAMEPAD, next);
            true
        }
        _ => false,
    }
}
