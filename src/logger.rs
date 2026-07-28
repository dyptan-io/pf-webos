//! Routes `tracing` to TCP (dev machine) or log file. Destination from argv[1] at
//! runtime (webOS SAM passes launch `params` as JSON argv), not compile-time.
use std::collections::VecDeque;
use std::io::{Seek, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Filter, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, Layer, Registry};

use crate::store::LogLevelOverride;

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

/// Launch-time log level override from `TELEMETRY_LEVEL` env var.
/// Folded into settings so Diagnostics can display it. `None` leaves persisted level.
pub fn launch_level_override() -> Option<LogLevelOverride> {
    match launch_params().telemetry_level.as_deref()?.to_ascii_lowercase().as_str() {
        "debug" => Some(LogLevelOverride::Debug),
        "info" => Some(LogLevelOverride::Info),
        "warn" => Some(LogLevelOverride::Warn),
        "error" => Some(LogLevelOverride::Error),
        _ => None,
    }
}

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// Log destination (file or TCP). Non-blocking dispatch prevents blocking video pump.
enum Sink {
    File { file: std::fs::File, written: u64 },
    Tcp(TcpStream),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::File { file, written } => {
                let n = file.write(buf)?;
                *written += n as u64;
                Ok(n)
            }
            Self::Tcp(s) => s.write(buf),
        }
    }

    /// `tracing_appender`'s worker thread flushes after each drained batch, not per
    /// line — so the wrap check runs once per batch instead of once per write.
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File { file, written } => {
                file.flush()?;
                if *written >= MAX_LOG_BYTES {
                    file.set_len(0)?;
                    file.seek(std::io::SeekFrom::Start(0))?;
                    *written = 0;
                }
                Ok(())
            }
            Self::Tcp(s) => s.flush(),
        }
    }
}

/// Truncated on every launch, then wiped and restarted mid-run if it hits
/// `MAX_LOG_BYTES` — bounds disk usage regardless of session length.
fn open_file(app_dir: &Path) -> Result<Sink> {
    let path = app_dir.join(format!("punktfunk-webos-{VERSION}.log"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("open log file {}", path.display()))?;
    Ok(Sink::File { file, written: 0 })
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

/// Startup filter level, mapped from persisted/launch-override settings.
pub fn resolved_level() -> tracing::Level {
    override_to_level(crate::store::load_settings().log_level_override)
}

fn override_to_level(o: LogLevelOverride) -> tracing::Level {
    match o {
        LogLevelOverride::Debug => tracing::Level::DEBUG,
        LogLevelOverride::Info => tracing::Level::INFO,
        LogLevelOverride::Warn => tracing::Level::WARN,
        LogLevelOverride::Error => tracing::Level::ERROR,
    }
}

/// Live reload handle for dynamic level changes (Diagnostics screen).
static LEVEL_HANDLE: OnceLock<reload::Handle<LevelFilter, Registry>> = OnceLock::new();

/// Mirrors level for `RingBufferLayer` (`reload::Layer` isn't Clone for two-layer attachment).
static RING_LEVEL: AtomicU8 = AtomicU8::new(3);

fn level_ordinal(level: tracing::Level) -> u8 {
    match level {
        tracing::Level::ERROR => 1,
        tracing::Level::WARN => 2,
        tracing::Level::INFO => 3,
        tracing::Level::DEBUG => 4,
        tracing::Level::TRACE => 5,
    }
}

/// Applies immediately from Diagnostics screen; no-op before `init_subscriber`.
pub fn set_level_override(level: LogLevelOverride) {
    let resolved = override_to_level(level);
    RING_LEVEL.store(level_ordinal(resolved), Ordering::Relaxed);
    if let Some(handle) = LEVEL_HANDLE.get() {
        let _ = handle.modify(|filter| *filter = LevelFilter::from_level(resolved));
    }
}

/// Bounds overlay memory and per-event lock scope (see `RingBufferLayer`).
const RING_CAPACITY: usize = 200;
const RING_LINE_MAX_CHARS: usize = 200;

static RING_BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn ring_buffer() -> &'static Mutex<VecDeque<String>> {
    RING_BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAPACITY)))
}

/// Whether `RingBufferLayer` captures (off by default). Toggled by Yellow button
/// cycle — sessions not using the overlay pay only one atomic load per event.
static RING_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Toggle ring-buffer capture on/off; stopping also clears the buffer.
pub fn set_ring_capture(active: bool) {
    RING_CAPTURE_ACTIVE.store(active, Ordering::Relaxed);
    if !active {
        let mut buf = ring_buffer().lock().unwrap_or_else(PoisonError::into_inner);
        *buf = VecDeque::new();
    }
}

/// Last `n` log lines, oldest first — for the in-stream/menu log-tail overlay
/// (`ui::tiles::render_log_overlay_tile`). Clones out of the ring buffer only;
/// never touches the file or TCP sink.
pub fn recent_lines(n: usize) -> Vec<String> {
    let buf = ring_buffer().lock().unwrap_or_else(PoisonError::into_inner);
    let skip = buf.len().saturating_sub(n);
    buf.iter().skip(skip).cloned().collect()
}

/// Gates `RingBufferLayer` via the `Filter` trait rather than `Layer::enabled`
/// directly: `Layer::enabled` returning `false` short-circuits the *entire*
/// subscriber stack (see `Filtered`'s own docs — this is precisely why it
/// exists), which silently dropped every event — including to the file/TCP
/// sink — whenever the ring capture was inactive (i.e. almost always, since the
/// log overlay is off by default). A `Filter` disables only its own layer;
/// `fmt_layer` still sees and writes every event regardless of overlay state.
struct RingBufferFilter;

impl<S> Filter<S> for RingBufferFilter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _cx: &tracing_subscriber::layer::Context<'_, S>) -> bool {
        RING_CAPTURE_ACTIVE.load(Ordering::Relaxed)
            && level_ordinal(*metadata.level()) <= RING_LEVEL.load(Ordering::Relaxed)
    }

    /// Keep the hint bounded to the current ring/file level (`set_level_override`).
    /// An unbounded filter lowers tracing's global static max-level, forcing
    /// extra per-event callsite checks (down to `trace!`) instead of cached `never`.
    /// `handle.modify` in `set_level_override` refreshes interest when this changes.
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(match RING_LEVEL.load(Ordering::Relaxed) {
            1 => LevelFilter::ERROR,
            2 => LevelFilter::WARN,
            3 => LevelFilter::INFO,
            4 => LevelFilter::DEBUG,
            _ => LevelFilter::TRACE,
        })
    }
}

/// In-memory ring for log-tail overlay (independent of file/TCP sink).
/// Formats before lock, holds only for bounded push/pop; zero I/O in render path.
struct RingBufferLayer;

impl<S: tracing::Subscriber> Layer<S> for RingBufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        struct MessageVisitor(String);
        impl tracing::field::Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                if field.name() == "message" {
                    let _ = write!(self.0, "{value:?}");
                } else {
                    let _ = write!(self.0, " {}={value:?}", field.name());
                }
            }
        }
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        let line: String = format!("{:<5} {}", event.metadata().level(), visitor.0)
            .chars()
            .take(RING_LINE_MAX_CHARS)
            .collect();
        let mut buf = ring_buffer().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if buf.len() >= RING_CAPACITY {
            buf.pop_front();
        }
        buf.push_back(line);
    }
}

/// Build non-blocking writer for `tracing_subscriber`. Guard must stay alive for process lifetime.
fn writer(
    app_dir: &Path,
) -> Result<(
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    let sink = open_sink(app_dir).context("open log sink")?;
    Ok(tracing_appender::non_blocking(sink))
}

/// Builds writer, installs global subscriber (file/TCP + ring, shared level filter).
/// Returns `WorkerGuard` (must stay alive for process lifetime).
pub fn init_subscriber(app_dir: &Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let (writer, guard) = writer(app_dir)?;
    let level = resolved_level();
    let (filter, handle) = reload::Layer::new(LevelFilter::from_level(level));
    let _ = LEVEL_HANDLE.set(handle);
    RING_LEVEL.store(level_ordinal(level), std::sync::atomic::Ordering::Relaxed);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .with_filter(filter);
    // `RingBufferLayer` is gated by its own `RingBufferFilter` via `.with_filter`
    // (see that type's docs) so an inactive overlay can't silence `fmt_layer`.
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(RingBufferLayer.with_filter(RingBufferFilter))
        .init();
    Ok(guard)
}
