//! Minimal one-shot Luna (webOS service bus) caller, used by [`crate::dualsense`].
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
//! [`crate::dualsense::Feedback`]). Never call this from the render/input loop.
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
/// handed back (see `crate::dualsense::Feedback::release`), and a send that hasn't answered in
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
    if !available() {
        anyhow::bail!("luna-send-pub unavailable");
    }
    let mut child = Command::new(LUNA_SEND_PUB)
        .args(["-n", "1", uri, payload])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let deadline = Instant::now() + CALL_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => anyhow::bail!("luna-send-pub exited {status}"),
            None if Instant::now() >= deadline => {
                // Kill AND reap: an unreaped child becomes a zombie held for the life of
                // the app, and this path repeats for every send once a call starts hanging.
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("luna-send-pub timed out after {CALL_TIMEOUT:?}");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
