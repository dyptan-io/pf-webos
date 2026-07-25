//! Picks where `tracing` writes: a TCP stream to a dev machine when `task deploy`
//! passed a `telemetry` destination at launch, otherwise a versioned file under the
//! app's own writable directory (see `store.rs`'s module docs on that directory).
//! Call sites everywhere else just use plain `tracing::info!`/`warn!`/`debug!`/
//! `error!` — no handle to thread through; see `main.rs::run` for the one-time
//! subscriber setup this feeds.
//!
//! The destination is read from `argv[1]` at runtime, not baked in at compile time:
//! webOS's SAM launch passes the `params` object given to
//! `luna://com.webos.applicationManager/launch` to a native app as a JSON-encoded
//! first command-line argument on initial launch (confirmed in the webOS OSE native
//! app docs) — so `task deploy TELEMETRY=<host:port>` just threads it through that
//! `luna-send` call instead of requiring a rebuild per destination.
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Matches `ui.rs`'s version marker: `PKG_VERSION` is threaded in at Docker build
/// time by the Taskfile (`Cargo.toml` itself stays a fixed `0.0.1`); falls back to
/// `CARGO_PKG_VERSION` for a plain native `cargo build`.
const VERSION: &str = match option_env!("PKG_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// `argv[1]`'s shape, as SAM hands it to a native app on initial launch — see this
/// module's docs. Both fields optional: a plain launch (no `params`, or a `params`
/// with neither key — e.g. a future unrelated use of `params`) just means "no
/// telemetry", not a parse error.
#[derive(Deserialize, Default)]
struct LaunchParams {
    /// `host:port` of the dev machine's listener.
    telemetry: Option<String>,
    /// `debug`/`info`/`warn`/`error` — parsed via `tracing::Level`'s own `FromStr`.
    telemetry_level: Option<String>,
}

/// Parsed once (argv doesn't change over the process lifetime) and cached — every
/// caller below reads through this instead of re-parsing.
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

/// Defaults to `debug` — a configured TCP sink is an explicit opt-in for a dev
/// session, so send everything unless told otherwise.
fn telemetry_level() -> tracing::Level {
    launch_params()
        .telemetry_level
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(tracing::Level::DEBUG)
}

/// Where log lines actually go — a plain `Write` impl handed to
/// `tracing_appender::non_blocking`, which owns the buffering/background-thread
/// dispatch (see `main.rs::run`) so a slow disk or a dev machine not draining its
/// listener fast enough never blocks the caller, in particular the video-pump
/// thread.
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

/// Truncate (not append) — this file previously grew unbounded across every launch
/// for the life of the install; each run's log now starts fresh. Info-and-above
/// only (no debug) so on-device disk usage stays bounded across ordinary
/// sideloaded runs with no telemetry destination configured.
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

/// `app_dir` is the app's own writable directory (`store::app_dir()`).
///
/// A configured address that's unreachable (dev machine off the network, `task
/// deploy` exited before this launch) falls back to the file sink rather than
/// failing the whole app over what's meant to be a dev convenience.
fn open_sink(app_dir: &Path) -> Result<Sink> {
    if let Some(addr) = telemetry_addr() {
        if let Ok(stream) = TcpStream::connect(addr) {
            return Ok(Sink::Tcp(stream));
        }
    }
    open_file(app_dir)
}

/// The level the installed subscriber should filter at — `debug` (or whatever
/// `telemetry_level` was given) when a telemetry address was configured, `info`
/// for the plain on-device file (see `open_file`'s docs on why).
pub fn resolved_level() -> tracing::Level {
    if telemetry_addr().is_some() {
        telemetry_level()
    } else {
        tracing::Level::INFO
    }
}

/// Builds the writer `main.rs::run` installs into `tracing_subscriber::fmt()`.
/// Returns the `non_blocking` writer plus its `WorkerGuard` — the guard must be
/// kept alive for the process lifetime (dropping it stops the background writer
/// thread and flushes whatever's queued).
pub fn init(
    app_dir: &Path,
) -> Result<(
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    let sink = open_sink(app_dir).context("open log sink")?;
    Ok(tracing_appender::non_blocking(sink))
}
