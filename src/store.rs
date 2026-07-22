//! Persisted client identity, known hosts, and stream settings — JSON files under
//! the app's own writable directory (`$HOME`, e.g.
//! `/media/developer/apps/usr/palm/applications/io.dyptan.punktfunk.webos/`). Mirrors
//! `pf-client-core::trust`'s file-per-concern layout (identity PEMs / known-hosts
//! JSON / settings JSON) so the shape is familiar, trimmed to what this client uses.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn app_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from)
}

fn identity_paths() -> (PathBuf, PathBuf) {
    let dir = app_dir();
    (dir.join("client-cert.pem"), dir.join("client-key.pem"))
}

/// Loads the persisted client identity, generating and saving a new one on first run.
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
    /// `None` = discovered but never paired.
    pub fingerprint: Option<[u8; 32]>,
    /// The management API's port (game library fetch) — `#[serde(default)]` so a
    /// `known-hosts.json` saved before this field existed still loads. `None` falls
    /// back to `library::DEFAULT_MGMT_PORT`.
    #[serde(default)]
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MAC(s) (`aa:bb:cc:dd:ee:ff`), learned from this host's mDNS `mac`
    /// TXT while it was last seen awake (see `discovery::DiscoveredHost::mac` and
    /// `app::App::drain_discovery`). Empty if never learned — a host in that state
    /// can't be woken, so `app.rs` falls back to the plain unreachable message.
    #[serde(default)]
    pub mac: Vec<String>,
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

pub fn save_known_hosts(hosts: &[KnownHost]) -> Result<()> {
    let json = serde_json::to_string_pretty(hosts).context("serialize known hosts")?;
    std::fs::write(known_hosts_path(), json).context("write known-hosts.json")
}

/// Upserts by `(host, port)`, keeping the existing fingerprint if the new record
/// doesn't have one (a fresh mDNS discovery shouldn't clobber a paired fingerprint) —
/// same reasoning for `mac`, learned separately (see `App::drain_discovery`) and not
/// necessarily known again at the point something else re-upserts this host.
pub fn upsert_known_host(hosts: &mut Vec<KnownHost>, mut new: KnownHost) {
    if let Some(existing) = hosts.iter_mut().find(|h| h.host == new.host && h.port == new.port) {
        if new.fingerprint.is_none() {
            new.fingerprint = existing.fingerprint;
        }
        if new.mac.is_empty() {
            new.mac.clone_from(&existing.mac);
        }
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
    std::fs::write(selected_host_path(), json).context("write selected-host.json")
}

/// Video decode backend selectable in Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoBackend {
    /// NDL DirectMedia v2 — stable baseline, no `pauseAtDecodeTime`.
    #[default]
    Ndl,
    /// Starfish/SMP (`libplayerAPIs_C.so`) — `pauseAtDecodeTime` + smooth PTS pacing
    /// + `maxFrameRate`; better above 1080p, requires the bundled wrapper .so.
    Starfish,
}

/// Stream settings: resolution/framerate/bitrate/HDR/video-backend.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
    /// Whether a Wake-on-LAN magic packet is sent automatically (no prompt) when a
    /// known host turns out to be unreachable. Off by default — a first-time
    /// unreachable host always asks. There's deliberately no settings-screen row for
    /// this: it's toggled from the wake prompt itself (`app::App::handle_wake_event`),
    /// which is also the only place that re-surfaces if turning it on doesn't
    /// actually get the host back within a minute (see `app.rs`'s `tick_wake` docs).
    #[serde(default)]
    pub wol_auto_send: bool,
    /// Which hardware decode pipeline to use. Defaults to `Ndl` (stable baseline);
    /// switch to `Starfish` to test `pauseAtDecodeTime` + smooth-pacing above 1080p.
    /// Persisted across restarts; takes effect on the next stream.
    #[serde(default)]
    pub video_backend: VideoBackend,
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
            wol_auto_send: false,
            video_backend: VideoBackend::Ndl,
        }
    }
}

fn settings_path() -> PathBuf {
    app_dir().join("settings.json")
}

pub fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> Result<()> {
    let json = serde_json::to_string_pretty(settings).context("serialize settings")?;
    std::fs::write(settings_path(), json).context("write settings.json")
}

/// Test/dev override: a config file dropped alongside sideloading skips straight to
/// a connect target — see `punktfunk-webos-client` memory notes for why this exists
/// (no documented way to pass CLI args through a normal SAM launch). Still supported
/// for quick bring-up testing; the UI flow below is the normal path.
pub fn dev_override_connect() -> Option<(String, u16)> {
    let path = Path::new(&app_dir()).join("connect.conf");
    let content = std::fs::read_to_string(path).ok()?;
    let target = content.split_whitespace().nth(1)?;
    match target.split_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((target.to_string(), 9777)),
    }
}
