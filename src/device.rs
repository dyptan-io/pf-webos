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

/// TV capabilities detected at runtime (best-effort; missing sources fall back safely).
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// CPU cores (drives off-main-thread work before contention).
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

/// Whether platform decoder *declares* AV1 (not the same as ability to stream it).
/// WHY: G5 declares it but fails in Starfish/NDL. Weak signal; caller must also
/// require `dev_override_enable_av1`. Still needed as necessary condition + diagnostic.
pub fn supports_av1() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let needle = b"video/x-av1";
        let found = std::fs::read(LX_VIDEODEC_PLUGIN).is_ok_and(|data| data.windows(needle.len()).any(|w| w == needle));
        tracing::info!(
            "device: platform decoder {} AV1 ({LX_VIDEODEC_PLUGIN})",
            if found { "declares" } else { "does not declare" },
        );
        found
    })
}

/// Extract JSON field without parser (avoids serde on filesystem source).
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

    /// Log device details at startup (essential for bug reports on unknown models).
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
