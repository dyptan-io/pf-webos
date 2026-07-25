//! Runtime device capability detection.
//!
//! This client ships one binary to an open-ended set of TVs — a 2020 CX on webOS 5.6 and a
//! 2025 G5 on webOS 10.3 are both targets, and neither is the last one. Anything that
//! differs per model therefore has to be decided **at runtime**, from what the device
//! actually reports, rather than baked in.
//!
//! One thing deliberately **not** here: CPU codegen. `-C target-cpu` is a compile-time
//! flag, so a single `.ipk` cannot vary it per device — the baseline stays at the oldest
//! supported model and that is simply the cost of one binary. What *can* vary is
//! behaviour, and that is what this module feeds.
//!
//! **Detection is preferred by attempt, not by lookup table.** A table of model names is
//! wrong the day a TV ships that isn't in it. Where a capability can be probed by trying
//! it and handling failure (see `ndl::NdlVideo::load`'s audio fallback), that is always
//! the better mechanism; the facts here are for the decisions that can't be probed
//! cheaply, and for the log line that makes a bug report from an unknown model useful.

/// What this TV reports about itself. Every field is best-effort: a model that doesn't
/// expose a given source falls back to a safe default rather than failing.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// CPU cores usable by this process. Drives how much work can run off the main
    /// thread before it becomes contention rather than parallelism — a CX has 3, the
    /// G5 reports 2.
    pub cores: usize,
    /// Major webOS release (5, 6, … 10), when it can be determined.
    pub webos_major: Option<u32>,
    /// Marketing model string, e.g. `OLED65G58LW.DEUQLJP`. Diagnostics only — never
    /// branch on this, see the module docs.
    pub model: Option<String>,
}

/// webOS publishes these as plain JSON. Readable from a Dev-Mode shell; whether the
/// jailed app can read them varies, so every read is optional.
const OS_INFO: &str = "/var/run/nyx/os_info.json";
const DEVICE_INFO: &str = "/var/run/nyx/device_info.json";

/// The platform's real hardware video-decode element — both NDL and Starfish drive it
/// underneath (see docs/NOTES.md "The two backends are front-ends to the same
/// pipeline"). Its caps template strings declare exactly what the silicon decodes,
/// which makes it the authoritative per-model answer to "does this TV do AV1" without
/// loading a decoder to find out. World-readable under the SAM jail (`-rwxr-xr-x`,
/// confirmed on a G5).
const LX_VIDEODEC_PLUGIN: &str = "/usr/lib/gstreamer-1.0/libgstlxvideodec.so";

/// Whether the platform decoder *declares* AV1 (`video/x-av1` in its `GStreamer` caps).
///
/// **Declaring it is not the same as being able to stream it, and on the one device
/// tested it is not enough.** A G5 whose `libgstlxvideodec.so` advertises `video/x-av1`
/// cannot actually run an AV1 session: through Starfish the load either times out or
/// completes and then presents a black screen (and twice took the process down with it),
/// and NDL's implementation has no AV1 at all. So this answers "does the silicon claim
/// AV1", which turned out to be a much weaker statement than "can this TV play an AV1
/// stream" — the caller must additionally require
/// [`crate::store::dev_override_enable_av1`], and the negotiation is opt-in for that
/// reason. Kept because it is still a necessary condition and a useful diagnostic.
///
/// Fails closed: a missing/unreadable plugin reads as "no AV1". Memoized — the file is
/// ~400 KB and the answer can't change while the process runs.
pub fn supports_av1() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let needle = b"video/x-av1";
        let found = std::fs::read(LX_VIDEODEC_PLUGIN)
            .is_ok_and(|data| data.windows(needle.len()).any(|w| w == needle));
        tracing::info!(
            "device: platform decoder {} AV1 ({LX_VIDEODEC_PLUGIN})",
            if found { "declares" } else { "does not declare" },
        );
        found
    })
}

/// Pulls `"key": "value"` out of a flat JSON object without a parser — these two files
/// are flat and machine-generated, and this avoids handing `serde_json` a path that a
/// hostile-ish filesystem could make large.
fn json_str_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let rest = text.split_once(&needle)?.1;
    let rest = rest.split_once(':')?.1;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    let (value, _) = rest.split_once('"')?;
    Some(value.to_string())
}

impl DeviceInfo {
    pub fn detect() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let os = std::fs::read_to_string(OS_INFO).unwrap_or_default();
        let webos_major =
            json_str_field(&os, "webos_release").and_then(|v| v.split('.').next().and_then(|m| m.parse().ok()));
        let model = std::fs::read_to_string(DEVICE_INFO)
            .ok()
            .and_then(|t| json_str_field(&t, "product_id"));
        Self {
            cores,
            webos_major,
            model,
        }
    }

    /// Log everything known about the device once at startup.
    ///
    /// This exists so a report from a model neither developer owns is actionable: the
    /// first question about any playback problem is "what is this running on", and
    /// without this line the log answers it only indirectly.
    pub fn log(&self) {
        tracing::info!(
            "device: cores={} webos={} model={}",
            self.cores,
            self.webos_major
                .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
            self.model.as_deref().unwrap_or("unknown"),
        );
    }
}
