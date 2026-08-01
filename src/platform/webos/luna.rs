//! Minimal one-shot Luna (webOS service bus) caller, used by [`crate::platform::webos::dualsense`].
//!
//! **Why a subprocess and not `libluna-service2` directly.** In-process `LSCall` needs a
//! registered `LSHandle` attached to a running `GMainLoop`, and — the deciding factor — LS2
//! authorizes a caller by *which executable* is calling: permissions come from the role file
//! matched to the binary's path (`/usr/share/luna-service2/roles.d/`). `luna-send-pub`'s own
//! role is what was verified on-device to reach the Bluetooth HID methods from a dev-mode
//! install; an app registering under its own name is a different, unverified client identity.
//! Borrowing the tool's identity is the difference between "works on a non-rooted TV" and
//! "works only where someone already granted our appid the `devices` group".
//!
//! Cost is one fork/exec per call, which is why nothing here is on a hot path: the only
//! caller coalesces to the latest state and sends from its own thread (see
//! [`crate::platform::webos::dualsense::Feedback`]). Never call this from the render/input loop.
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Public-bus variant deliberately: on webOS 5.x-10.x `/usr/bin/luna-send` is `0700 root`
/// and unusable from an app's uid, while `luna-send-pub` is `0755` (verified on webOS 10.3).
const LUNA_SEND_PUB: &str = "/usr/bin/luna-send-pub";

/// A call that never answers must not wedge the caller's thread — `getReport` on a
/// disconnected pad does exactly that (verified on-device), so every child gets a deadline
/// and a kill rather than an unbounded wait.
///
/// Kept short deliberately: this bounds how long session teardown can wait for the pad to be
/// handed back (see `crate::platform::webos::dualsense::Feedback::release`), and a send that hasn't answered in
/// this long means the Bluetooth service is wedged or the pad is gone — in which case there is
/// nothing left to hand back. A healthy send answers in tens of milliseconds.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_millis(800);

/// Whether the public-bus tool exists and is executable, probed once. A TV without it
/// (or an install whose jail hides it) simply gets no `DualSense` feedback — every caller
/// treats this as "feature absent", never as an error worth surfacing.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let ok = std::fs::metadata(LUNA_SEND_PUB).is_ok_and(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        });
        if !ok {
            tracing::info!("{LUNA_SEND_PUB} not executable here — DualSense feedback disabled");
        }
        ok
    })
}

/// Fires one `luna://` call and discards the reply. Blocking (bounded by [`CALL_TIMEOUT`]);
/// `Err` covers spawn failure, the deadline, and a non-zero exit.
///
/// The reply is discarded rather than parsed because the useful failures are not in it:
/// a wrong payload shape answers `returnValue:false` with an error code, which is a bug to
/// fix during bring-up, not a runtime condition to branch on. Feedback is idempotent — the
/// next state update re-sends everything — so a dropped call needs no recovery path.
pub fn call(uri: &str, payload: &str) -> anyhow::Result<()> {
    run_bounded(&["-n", "1", uri, payload], CALL_TIMEOUT, false).map(|_| ())
}

/// Like [`call`] but waits for and returns the reply JSON on stdout — for the callers that
/// must read a value back (e.g. `getSystemSettings`, or an `exec` service that returns command
/// output). Non-interactive `luna-send-pub` only waits for the reply when given `-w` (without
/// it, it fires and exits before the reply arrives — verified on-device, webOS 10.3), so the
/// caller's `timeout` is passed through as `-w` AND, plus a margin, as the kill deadline.
pub fn call_capture(uri: &str, payload: &str, timeout: Duration) -> anyhow::Result<String> {
    let wait_ms = timeout.as_millis().to_string();
    // Own deadline sits past `-w` so `luna-send-pub` exits on its own timeout first.
    run_bounded(
        &["-n", "1", "-w", &wait_ms, uri, payload],
        timeout + Duration::from_millis(500),
        true,
    )
}

/// Spawns `luna-send-pub` with `args`, polling until it exits or `timeout` elapses (killing and
/// reaping on timeout so no zombie is left). Returns its stdout when `capture` is set, else an
/// empty string. Shared by [`call`] and [`call_capture`] — the only differences between them are
/// the args, `capture`, and the deadline.
fn run_bounded(args: &[&str], timeout: Duration, capture: bool) -> anyhow::Result<String> {
    if !available() {
        anyhow::bail!("luna-send-pub unavailable");
    }
    let mut child = Command::new(LUNA_SEND_PUB)
        .args(args)
        .stdin(Stdio::null())
        .stdout(if capture { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::null())
        .spawn()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => {
                let out = if capture {
                    String::from_utf8_lossy(&child.wait_with_output()?.stdout).into_owned()
                } else {
                    String::new()
                };
                return Ok(out);
            }
            Some(status) => anyhow::bail!("luna-send-pub exited {status}"),
            None if Instant::now() >= deadline => {
                // Kill AND reap: an unreaped child becomes a zombie held for the life of
                // the app, and this path repeats for every send once a call starts hanging.
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("luna-send-pub timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
