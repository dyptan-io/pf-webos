//! Routes `tracing` to TCP (dev machine) or log file. Destination from argv[1] at
//! runtime (webOS SAM passes launch `params` as JSON argv), not compile-time.
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

/// `PKG_VERSION` from Docker/Taskfile; falls back to `CARGO_PKG_VERSION`.
const VERSION: &str = match option_env!("PKG_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// argv[1] shape from SAM; both fields optional (no error if missing).
#[derive(Deserialize, Default)]
struct LaunchParams {
    telemetry: Option<String>,
    telemetry_level: Option<String>,
}

/// Cache launch params once; argv doesn't change over process lifetime.
fn launch_params() -> &'static LaunchParams {
    static PARAMS: OnceLock<LaunchParams> = OnceLock::new();
    PARAMS.get_or_init(|| {
        std::env::args()
            .nth(1)
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    })
}

fn telemetry_addr() -> Option<&'static str> {
    launch_params().telemetry.as_deref().filter(|s| !s.is_empty())
}

/// Default to debug; TCP is opt-in for dev, so send everything by default.
fn telemetry_level() -> tracing::Level {
    launch_params()
        .telemetry_level
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(tracing::Level::DEBUG)
}

/// Log destination (file or TCP). Non-blocking dispatch prevents blocking video pump.
enum Sink {
    File(std::fs::File),
    Tcp(TcpStream),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::File(f) => f.write(buf),
            Self::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File(f) => f.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

/// Truncate log file per run (was unbounded). Info-only to bound disk usage without telemetry.
fn open_file(app_dir: &Path) -> Result<Sink> {
    let path = app_dir.join(format!("punktfunk-webos-{VERSION}.log"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("open log file {}", path.display()))?;
    Ok(Sink::File(file))
}

/// Open TCP or file sink; fall back to file if unreachable (dev convenience, not critical).
fn open_sink(app_dir: &Path) -> Result<Sink> {
    if let Some(addr) = telemetry_addr() {
        if let Ok(stream) = TcpStream::connect(addr) {
            return Ok(Sink::Tcp(stream));
        }
    }
    open_file(app_dir)
}

/// Subscriber filter level: debug if telemetry configured, info for on-device file.
pub fn resolved_level() -> tracing::Level {
    if telemetry_addr().is_some() {
        telemetry_level()
    } else {
        tracing::Level::INFO
    }
}

/// Build non-blocking writer for `tracing_subscriber`. Guard must stay alive for process lifetime.
pub fn init(
    app_dir: &Path,
) -> Result<(
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    let sink = open_sink(app_dir).context("open log sink")?;
    Ok(tracing_appender::non_blocking(sink))
}
