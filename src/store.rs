//! Persisted identity (PEMs), known hosts, and settings (JSON). Layout mirrors `pf-client-core::trust`.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) fn app_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from)
}

fn identity_paths() -> (PathBuf, PathBuf) {
    let dir = app_dir();
    (dir.join("client-cert.pem"), dir.join("client-key.pem"))
}

/// Load or generate identity (on first run).
pub fn load_or_create_identity() -> Result<(String, String)> {
    let (cert_path, key_path) = identity_paths();
    if let (Ok(cert), Ok(key)) = (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path)) {
        return Ok((cert, key));
    }
    let identity = punktfunk_core::quic::endpoint::generate_identity().context("generate_identity")?;
    std::fs::write(&cert_path, &identity.0).context("write client-cert.pem")?;
    std::fs::write(&key_path, &identity.1).context("write client-key.pem")?;
    Ok(identity)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownHost {
    pub name: String,
    pub host: String,
    pub port: u16,
    /// None = discovered but never paired.
    pub fingerprint: Option<[u8; 32]>,
    /// Management API port (game library); defaults to `library::DEFAULT_MGMT_PORT`.
    #[serde(default)]
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MACs learned from mDNS; empty if never advertised.
    #[serde(default)]
    pub mac: Vec<String>,
    /// Auto-wake on unreachable (per-host, off by default; lives in Wake settings).
    #[serde(default)]
    pub wol_auto: bool,
    /// Pinned game IDs (up to `MAX_PINNED_GAMES`).
    #[serde(default)]
    pub pinned: Vec<String>,
}

/// Max games pinned to one host's always-visible grid row at once.
pub const MAX_PINNED_GAMES: usize = 5;

/// Pin ID for "Desktop" card (stored in pinned like games; counts toward `MAX_PINNED_GAMES`).
pub const DESKTOP_PIN_ID: &str = "__desktop__";

impl KnownHost {
    pub fn is_pinned(&self, id: &str) -> bool {
        self.pinned.iter().any(|p| p == id)
    }

    /// Whether toggling id would do anything (unpin always ok, pin only if under `MAX_PINNED_GAMES`).
    pub fn can_toggle_pin(&self, id: &str) -> bool {
        self.is_pinned(id) || self.pinned.len() < MAX_PINNED_GAMES
    }

    /// Toggles `id`'s pinned state (a `GameEntry::id`, or `DESKTOP_PIN_ID`) —
    /// a no-op when `can_toggle_pin` is false.
    pub fn toggle_pin(&mut self, id: &str) {
        match self.pinned.iter().position(|p| p == id) {
            Some(i) => drop(self.pinned.remove(i)),
            None if self.can_toggle_pin(id) => self.pinned.push(id.to_string()),
            None => {}
        }
    }
}

fn known_hosts_path() -> PathBuf {
    app_dir().join("known-hosts.json")
}

pub fn load_known_hosts() -> Vec<KnownHost> {
    std::fs::read_to_string(known_hosts_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write-then-rename, never truncate-in-place: `std::fs::write` truncates first,
/// so a kill/power-cut mid-write (this is a TV — losing power IS the off switch)
/// leaves a half-file, and the loaders' `.ok().unwrap_or_default()` would then
/// silently discard every paired host / all settings. A rename on the same
/// filesystem is atomic; readers see the old file or the new one, never a torn one.
fn write_atomic(path: std::path::PathBuf, contents: &str, what: &'static str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("write {what} (tmp)"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {what} into place"))
}

pub fn save_known_hosts(hosts: &[KnownHost]) -> Result<()> {
    let json = serde_json::to_string_pretty(hosts).context("serialize known hosts")?;
    write_atomic(known_hosts_path(), &json, "known-hosts.json")
}

/// Upserts by `(host, port)`, keeping the existing fingerprint if the new record
/// doesn't have one (a fresh mDNS discovery shouldn't clobber a paired fingerprint) —
/// same reasoning for `mac`, learned separately (see `App::drain_discovery`) and not
/// necessarily known again at the point something else re-upserts this host. `pinned`
/// is *always* kept from the existing record — only `KnownHost::toggle_pin` ever
/// changes it, so no add/edit/re-pair flow may clobber it.
pub fn upsert_known_host(hosts: &mut Vec<KnownHost>, mut new: KnownHost) {
    if let Some(existing) = hosts.iter_mut().find(|h| h.host == new.host && h.port == new.port) {
        if new.fingerprint.is_none() {
            new.fingerprint = existing.fingerprint;
        }
        if new.mac.is_empty() {
            new.mac.clone_from(&existing.mac);
        }
        new.pinned.clone_from(&existing.pinned);
        // Per-host preference, not something a re-pair/re-add should reset.
        new.wol_auto = existing.wol_auto;
        *existing = new;
    } else {
        hosts.push(new);
    }
}

fn selected_host_path() -> PathBuf {
    app_dir().join("selected-host.json")
}

/// The sidebar host row the user last had active — so relaunching the app lands
/// back on its game grid instead of an unfocused sidebar. `(host, port)`, not an
/// index: `known_hosts` order isn't stable across a forget/re-add.
#[derive(Clone, Serialize, Deserialize)]
struct SelectedHost {
    host: String,
    port: u16,
}

pub fn load_selected_host() -> Option<(String, u16)> {
    let s = std::fs::read_to_string(selected_host_path()).ok()?;
    let sel: SelectedHost = serde_json::from_str(&s).ok()?;
    Some((sel.host, sel.port))
}

pub fn save_selected_host(host: &str, port: u16) -> Result<()> {
    let json = serde_json::to_string_pretty(&SelectedHost {
        host: host.to_string(),
        port,
    })
    .context("serialize selected host")?;
    write_atomic(selected_host_path(), &json, "selected-host.json")
}

/// Video decode backend selectable in Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoBackend {
    /// NDL `DirectMedia` v2 — stable baseline, no `pauseAtDecodeTime`.
    #[default]
    Ndl,
    /// Starfish/SMP (`libplayerAPIs_C.so`) — `pauseAtDecodeTime` + smooth PTS pacing
    /// + `maxFrameRate`; better above 1080p, requires the bundled wrapper .so.
    Starfish,
}

/// Codec preference selectable in Settings — a *preference*, not a demand. The host
/// resolves the session codec from the client's advertised set via its own precedence
/// ladder (HEVC > AV1 > H.264), honouring the preference only when its encoder can
/// actually produce it — so "H264" on a host that can't encode H.264 still gets HEVC
/// rather than no session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecPref {
    /// No preference — the host's precedence ladder decides (HEVC in practice).
    #[default]
    Auto,
    H264,
    Hevc,
    /// AV1 — only decodable through the Starfish backend (the platform decoder
    /// advertises `video/x-av1`; NDL's impl does not — see docs/NOTES.md), so this
    /// choice is only offered/advertised when `video_backend` is `Starfish`.
    Av1,
}

/// Override for the VUI `video_full_range_flag` sent to the decoder — see
/// `session::connect`'s colour-info splice. `Auto` forwards the host's own
/// `ColorInfo.full_range` unchanged; `Full`/`Limited` force it, to test whether the
/// panel's own default (rather than what the stream signals) is what's washing out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorRangeOverride {
    #[default]
    Auto,
    Full,
    Limited,
}

/// Override for the on-device log verbosity, settable live from the Diagnostics
/// screen — see `logger::set_level_override`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevelOverride {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

/// Stream settings: resolution/framerate/bitrate/HDR/video-backend.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    /// Refresh rate (30/60/120) — sent to the host as the exact wire `Mode.refresh_hz`.
    pub refresh_hz: u32,
    /// `0` (Automatic — `punktfunk_core`'s own client-side AIMD bitrate controller, see
    /// `ui::BITRATE_AUTOMATIC`) or 10_000-150_000 (10-150 Mbps) fixed, adjusted via the settings
    /// slider — see `ui::BITRATE_MIN_KBPS`/`BITRATE_MAX_KBPS`.
    pub bitrate_kbps: u32,
    pub hdr_enabled: bool,
    /// Which hardware decode pipeline to use. Defaults to `Ndl` (stable baseline);
    /// switch to `Starfish` to test `pauseAtDecodeTime` + smooth-pacing above 1080p.
    /// Persisted across restarts; takes effect on the next stream.
    #[serde(default)]
    pub video_backend: VideoBackend,
    /// Preferred session codec — see [`CodecPref`]. `Av1` is additionally gated on the
    /// Starfish backend and the platform decoder actually declaring AV1
    /// (`device::supports_av1`), both in the picker and again at connect time.
    #[serde(default)]
    pub codec: CodecPref,
    /// Whether the in-stream stats overlay (resolution/codec, measured fps, drops,
    /// decoder feed time) is drawn in the top-right corner during a stream. Off by
    /// default; takes effect on the next stream.
    #[serde(default)]
    pub stats_overlay: bool,
    /// Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to
    /// what it can actually capture, and the *resolved* count drives the decoder and
    /// playback layout — `audio.rs` has always handled up to 8; only the request was
    /// pinned at stereo. `#[serde(default …)]` so an existing settings.json loads as 2.
    #[serde(default = "default_audio_channels")]
    pub audio_channels: u8,
    /// Forces the VUI range flag sent to the decoder regardless of what the host
    /// signals — see [`ColorRangeOverride`]. Debug aid for the washed-out-colour
    /// investigation; takes effect on the next connect.
    #[serde(default)]
    pub color_range_override: ColorRangeOverride,
    /// On-device log verbosity — see [`LogLevelOverride`]. Persisted, so a user's
    /// choice in Diagnostics survives restarts (fresh install defaults to `Info`);
    /// applied live via `logger::set_level_override` the moment it's changed. A
    /// `TELEMETRY_LEVEL` launch (`logger::launch_level_override`) still overrides
    /// the persisted value for that run — see `load_settings`.
    #[serde(default)]
    pub log_level_override: LogLevelOverride,
    /// Diagnostics' "Show logs" toggle, applied at startup (`App::new`). Distinct
    /// from the Yellow-button overlay cycle (`main.rs`'s `LOG_OVERLAY_STATE`),
    /// which stays ephemeral and never writes here.
    #[serde(default)]
    pub show_logs: bool,
}

fn default_audio_channels() -> u8 {
    2
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            width: 3840,
            height: 2160,
            refresh_hz: 60,
            // Automatic: a fixed number, however carefully picked (aurora-tv's own
            // moonlight-tv wiki calls ~35-40 Mbps the practical sweet spot for this decode
            // path), never adapts to a link that degrades mid-session the way punktfunk's
            // own client-side AIMD controller does — see `ui::BITRATE_AUTOMATIC`.
            bitrate_kbps: 0,
            hdr_enabled: true,
            stats_overlay: false,
            video_backend: VideoBackend::Ndl,
            codec: CodecPref::Auto,
            audio_channels: default_audio_channels(),
            color_range_override: ColorRangeOverride::Auto,
            log_level_override: LogLevelOverride::Info,
            show_logs: false,
        }
    }
}

fn settings_path() -> PathBuf {
    app_dir().join("settings.json")
}

pub fn load_settings() -> Settings {
    let mut settings: Settings = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // `task deploy TELEMETRY=...` dev convenience: TELEMETRY_LEVEL picks the level
    // this launch starts at (and what Diagnostics displays), overriding whatever
    // was last persisted — see `logger::launch_level_override`. Absent, the
    // persisted value stands (Info on a fresh install).
    if let Some(level) = crate::logger::launch_level_override() {
        settings.log_level_override = level;
    }
    settings
}

pub fn save_settings(settings: &Settings) -> Result<()> {
    let json = serde_json::to_string_pretty(settings).context("serialize settings")?;
    write_atomic(settings_path(), &json, "settings.json")
}

/// Persists `Settings` on a dedicated background thread instead of the caller's —
/// `save_settings`'s write-then-rename blocks on real disk I/O (measured ~100-200ms
/// on-device), which is fine for the occasional save but was stalling the UI thread
/// on every single settings-row adjustment (bitrate slider steps, a toggle flip),
/// reading as input lag on the very controls someone expects to feel instant.
///
/// A single long-lived writer thread, not one spawn per save: rapid adjustments
/// (holding the bitrate slider) replace the pending value rather than queuing every
/// intermediate one, so a burst of changes costs one disk write of the final state,
/// not N — and, since one thread ever calls `save_settings`, writes can't complete
/// out of order the way N independently-spawned threads racing the filesystem could.
pub struct SettingsWriter {
    pending: std::sync::Arc<(std::sync::Mutex<Option<Settings>>, std::sync::Condvar)>,
}

impl SettingsWriter {
    pub fn spawn() -> Self {
        let state = std::sync::Arc::new((std::sync::Mutex::new(None::<Settings>), std::sync::Condvar::new()));
        let worker_state = state.clone();
        std::thread::spawn(move || {
            let (lock, cvar) = &*worker_state;
            loop {
                let mut guard = lock.lock().expect("settings-writer mutex poisoned");
                while guard.is_none() {
                    guard = cvar.wait(guard).expect("settings-writer mutex poisoned");
                }
                let settings = guard.take().expect("just checked Some");
                drop(guard);
                let _ = save_settings(&settings);
            }
        });
        Self { pending: state }
    }

    /// Queues `settings` to be written, replacing any not-yet-written value already
    /// queued. Returns immediately — never touches disk on the calling thread.
    pub fn save(&self, settings: Settings) {
        let (lock, cvar) = &*self.pending;
        *lock.lock().expect("settings-writer mutex poisoned") = Some(settings);
        cvar.notify_one();
    }
}

/// Test/dev override for NDL's undocumented frame-drop threshold: a single integer in
/// `$HOME/ndl-drop-threshold.conf`, absent by default.
///
/// Exists because the value's units aren't documented anywhere (the SDK header declares
/// `NDL_DirectVideoSetFrameDropThreshold` and stops), so it has to be swept against real
/// playback — and a full rebuild/redeploy per candidate value makes that impractical.
/// Same reasoning, and the same mechanism, as `dev_override_connect` below.
pub fn dev_override_ndl_drop_threshold() -> Option<i32> {
    let path = Path::new(&app_dir()).join("ndl-drop-threshold.conf");
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Opt **in** to handing NDL the Opus audio stream: `$HOME/ndl-audio-offload.conf`
/// containing `1`/`on`/`true`. Absent (the default) keeps NDL video-only and decodes
/// audio in-process.
///
/// **Off by default because on the one device it has been tested against it stops video
/// dead.** On an LG G5 (webOS 10.3) an audio-enabled `NDL_DirectMediaLoad` succeeds, then
/// every `NDL_DirectVideoPlay` keeps succeeding while the panel holds the first frame
/// forever — see `docs/NOTES.md`. That is strictly worse than the failure this feature's
/// own docs anticipated (silent audio): it takes the video plane with it, and it cannot be
/// detected the way the rest of this crate detects capabilities, because *the load
/// returns success*. Probe-by-attempt has nothing to catch.
///
/// The code stays, behind this file, because the offload is a real CPU saving on a 2-core
/// TV and may well work on other models — but a feature that has never been confirmed
/// working anywhere, and is confirmed to break the current target, cannot be the default.
/// Promote it to a real setting once some model is verified end-to-end.
pub fn dev_override_enable_ndl_audio_offload() -> bool {
    let path = Path::new(&app_dir()).join("ndl-audio-offload.conf");
    std::fs::read_to_string(path).is_ok_and(|s| matches!(s.trim(), "1" | "on" | "true"))
}

/// Opt **in** to offering AV1 at all: `$HOME/av1.conf` containing `1`/`on`/`true`.
///
/// **Off by default because AV1 has never produced a picture on the hardware this client
/// has been tested against.** The G5's platform decoder advertises `video/x-av1`
/// (`device::supports_av1`), and its host happily encodes AV1 — but through Starfish the
/// load either times out or completes and then shows a black screen with frames flowing,
/// and twice the process died outright; NDL cannot decode AV1 at all, and accepts the
/// stream silently if handed one. A codec that fails in three different ways is not a
/// menu option someone should be able to pick by accident, however good the bitrate
/// argument for it is.
///
/// The negotiation path stays wired (`CodecPref::Av1`, the `CODEC_AV1` advertisement,
/// the Starfish gating) so this is one file away from being testable on a device that
/// might do better. Promote it to a real setting when some TV plays an AV1 stream.
pub fn dev_override_enable_av1() -> bool {
    let path = Path::new(&app_dir()).join("av1.conf");
    std::fs::read_to_string(path).is_ok_and(|s| matches!(s.trim(), "1" | "on" | "true"))
}

/// Test/dev override: a config file dropped alongside sideloading skips straight to
/// a connect target — predates the finding (see `docs/NOTES.md`) that SAM launch
/// `params` reach a native app as `argv[1]` JSON on initial launch, which
/// `logger.rs` uses instead for telemetry. Still supported for quick bring-up
/// testing; the UI flow below is the normal path.
pub fn dev_override_connect() -> Option<(String, u16)> {
    let path = Path::new(&app_dir()).join("connect.conf");
    let content = std::fs::read_to_string(path).ok()?;
    let target = content.split_whitespace().nth(1)?;
    match target.split_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((target.to_string(), 9777)),
    }
}
