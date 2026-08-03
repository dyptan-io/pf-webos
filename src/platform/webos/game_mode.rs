//! Best-effort TV tuning for low input latency during a stream.
//!
//! webOS's own ALLM is an HDMI-plane feature — it never reaches a native app painting the
//! app/video plane, so the app-side equivalent is to drive the same TV settings ALLM would:
//! the Game *picture* mode (LG's "Game Optimizer", the real latency win), the Game *sound*
//! mode, and — for HDR — max Peak Brightness so an HDR game isn't dimmed.
//!
//! **Why via the Homebrew Channel and not `settingsservice` directly.** `com.webos.settingsservice`
//! grants the public bus NO access at all — `luna-send-pub` gets `Access denied` on both get
//! and set, and an app-identity call is "not privileged" (verified on-device, webOS 10.3). Only
//! a root private-bus caller may write these. So on a webosbrew-rooted set we hand the privileged
//! `luna-send` to `org.webosbrew.hbchannel.service`'s `exec` method, which runs it as root and
//! returns its stdout — reachable from the public bus. On a TV without the Homebrew Channel this
//! simply fails and is logged; the feature stays behind the Experimental toggle for that reason.
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

const URI_EXEC: &str = "luna://org.webosbrew.hbchannel.service/exec";
/// The Homebrew Channel's elevated helper — its presence is our proxy for "this TV is rooted",
/// since [`enter`] can only reach `settingsservice` by having this service run `luna-send` as
/// root (see the module docs). No service dir → not rooted → the feature can't work.
const HBCHANNEL_SERVICE: &str = "/media/developer/apps/usr/palm/services/org.webosbrew.hbchannel.service";

/// Whether this TV can drive picture/sound settings — i.e. it's webosbrew-rooted (the Homebrew
/// Channel elevated service is installed) and `luna-send-pub` exists to reach it. Probed once;
/// gates whether the Experimental "Game mode" row is offered at all.
pub fn is_rooted() -> bool {
    static ROOTED: OnceLock<bool> = OnceLock::new();
    *ROOTED
        .get_or_init(|| crate::platform::webos::luna::available() && std::path::Path::new(HBCHANNEL_SERVICE).is_dir())
}
/// Generous: the outer call forks `luna-send` as root on the TV, which itself round-trips to
/// `settingsservice`. Passed through as `luna-send-pub -w` and the process kill deadline.
const EXEC_TIMEOUT: Duration = Duration::from_millis(4000);

/// One setting we changed, plus the value to put back on exit (`None` = nothing to restore:
/// couldn't read the prior value, or it already equalled what we set).
pub struct Applied {
    category: &'static str,
    key: &'static str,
    restore_to: Option<String>,
}

/// SDR vs HDR have distinct picture-mode namespaces on LG panels — "game" is the SDR Game
/// preset, "hdrGame" the HDR one. Picking the wrong one leaves the panel in a non-game mode
/// once HDR engages, so this mirrors the *negotiated* HDR state (`session`'s `is_hdr`).
fn picture_mode(hdr: bool) -> &'static str {
    if hdr {
        "hdrGame"
    } else {
        "game"
    }
}

/// Runs `luna-send-cmd` as root through the Homebrew Channel's `exec` and returns the wrapped
/// command's stdout (the inner `luna-send`'s reply JSON). Errors if the exec service is absent
/// (non-rooted TV) or itself reports failure.
fn exec_root(luna_send_cmd: &str) -> anyhow::Result<String> {
    // serde_json builds the outer payload so `luna_send_cmd` (which contains quotes) is escaped
    // correctly regardless of the inner JSON.
    let payload = serde_json::json!({ "command": luna_send_cmd }).to_string();
    let reply = crate::platform::webos::luna::call_capture(URI_EXEC, &payload, EXEC_TIMEOUT)?;
    let parsed: Value = serde_json::from_str(&reply).map_err(|e| anyhow::anyhow!("exec reply parse: {e}"))?;
    if parsed.get("returnValue").and_then(Value::as_bool) != Some(true) {
        anyhow::bail!("hbchannel exec failed: {reply}");
    }
    Ok(parsed
        .get("stdoutString")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

/// The privileged `luna-send` for one settingsservice method, as a shell string for `exec_root`.
/// `body` is serialized by `serde_json` (like the outer payload) then single-quoted for the
/// shell; its own double quotes sit inside untouched.
fn luna_send(method: &str, body: &Value) -> String {
    format!("luna-send -n 1 luna://com.webos.settingsservice/{method} '{body}'")
}

/// Reads one setting's current string value so it can be put back on stream exit.
fn get_setting(category: &str, key: &str) -> Option<String> {
    let body = serde_json::json!({ "category": category, "keys": [key] });
    let out = exec_root(&luna_send("getSystemSettings", &body))
        .map_err(|e| tracing::warn!("game mode: read {category}.{key} failed: {e:#}"))
        .ok()?;
    serde_json::from_str::<Value>(&out)
        .ok()?
        .get("settings")?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

/// Sets one setting for the current source. No `dimension` — a native app is not an HDMI input,
/// so this targets the app/current context rather than a specific `hdmiN`. Confirms the inner
/// call's own `returnValue`, which `exec_root`'s success does not imply.
fn set_setting(category: &str, key: &str, value: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({ "category": category, "settings": { key: value } });
    let out = exec_root(&luna_send("setSystemSettings", &body))?;
    let ok = serde_json::from_str::<Value>(&out)
        .ok()
        .and_then(|v| v.get("returnValue").and_then(Value::as_bool))
        == Some(true);
    if !ok {
        anyhow::bail!("setSystemSettings rejected: {out}");
    }
    Ok(())
}

/// Reads the prior value, applies `value`, and returns an [`Applied`] carrying what to restore.
/// A failed set is logged and yields nothing to restore (the setting is unchanged).
fn apply(category: &'static str, key: &'static str, value: &str) -> Option<Applied> {
    let previous = get_setting(category, key);
    match set_setting(category, key, value) {
        Ok(()) => {
            tracing::info!("game mode: {category}.{key} -> {value} (was {previous:?})");
            Some(Applied {
                category,
                key,
                // A prior value equal to what we just set needs no restore write.
                restore_to: previous.filter(|p| p != value),
            })
        }
        Err(e) => {
            tracing::warn!("game mode: setting {category}.{key}={value} failed: {e:#}");
            None
        }
    }
}

/// Switches the TV into Game picture + sound mode for the stream (and, on HDR, max Peak
/// Brightness). Returns the changes to hand back to [`restore`]; empty if nothing applied.
pub fn enter(hdr: bool) -> Vec<Applied> {
    let mut applied = Vec::new();
    applied.extend(apply("picture", "pictureMode", picture_mode(hdr)));
    applied.extend(apply("sound", "soundMode", "game"));
    // Peak Brightness only matters for HDR here — it lifts an HDR game's highlights that the
    // panel would otherwise tone-map down; on SDR it's left as the user has it.
    if hdr {
        applied.extend(apply("picture", "peakBrightness", "high"));
    }
    applied
}

/// Restores every setting [`enter`] changed. No-op for entries with nothing to restore.
pub fn restore(applied: Vec<Applied>) {
    for a in applied {
        let Some(value) = a.restore_to else {
            continue;
        };
        match set_setting(a.category, a.key, &value) {
            Ok(()) => tracing::info!("game mode: {}.{} restored to {value}", a.category, a.key),
            Err(e) => tracing::warn!("game mode: restoring {}.{}={value} failed: {e:#}", a.category, a.key),
        }
    }
}
