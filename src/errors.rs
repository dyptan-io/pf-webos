//! User-facing sentences for `punktfunk-core`'s error and rejection types.
//!
//! Ported from `pf-client-core::trust`'s `connect_reject_message`/`pair_error_message` —
//! the same wording every other punktfunk client shows — rather than depending on that
//! crate (see `session.rs`'s module docs for why this client can't).
//!
//! Without this, failures rendered as Debug strings (e.g., "connect: Rejected(Busy)").
//! This translates them to user-facing sentences.
use punktfunk_core::reject::RejectReason;
use punktfunk_core::PunktfunkError;

/// Why the host turned this connection away.
pub fn reject_message(reason: RejectReason) -> String {
    match reason {
        RejectReason::Denied => "The host declined this device's request.".into(),
        RejectReason::ApprovalTimeout => {
            "Nobody approved the request on the host in time — approve this device on the host, \
             then try again."
                .into()
        }
        RejectReason::Superseded => {
            "A newer request from this device replaced this one — approve the latest request on \
             the host."
                .into()
        }
        RejectReason::IdentityRequired => {
            "The host requires pairing — pair this device (PIN or request access) first.".into()
        }
        RejectReason::PairingNotArmed => {
            "Pairing isn't armed on the host — arm it on the host's Pairing page, then try again.".into()
        }
        RejectReason::PairingBoundToOtherDevice => {
            "The host's pairing window is armed for a different device — arm it for this one.".into()
        }
        RejectReason::PairingRateLimited => {
            "Too many pairing attempts — wait a couple of seconds and try again.".into()
        }
        RejectReason::WireVersionMismatch => {
            "Client and host versions don't match — update both to the same release.".into()
        }
        RejectReason::Busy => "The host is busy with another session.".into(),
        RejectReason::SetupFailed => {
            "The host accepted the connection but couldn't start the stream — the host's own log \
             has the cause."
                .into()
        }
    }
}

/// Why connect/probe failed (distinguishes rejection from transport trouble).
pub fn connect_message(err: &PunktfunkError) -> String {
    match err {
        PunktfunkError::Rejected(reason) => reject_message(*reason),
        PunktfunkError::Timeout => "The host didn't answer. Is it running and reachable?".into(),
        PunktfunkError::Io(e) => format!(
            "Couldn't reach the host ({e}) — check that this TV and the host are on the same \
             network."
        ),
        PunktfunkError::Closed => "The host closed the connection.".into(),
        other => format!("Connection failed: {other}"),
    }
}

/// Why PIN pairing failed (Crypto = wrong PIN, not network problem).
pub fn pair_message(err: &PunktfunkError) -> String {
    match err {
        PunktfunkError::Crypto => "Wrong PIN — check the PIN on the host's Pairing page and try again.".into(),
        other => connect_message(other),
    }
}

/// Why a `GameStream` session ended, from the `ServerTermination` reason code the control stream
/// carried. Codes are NVST `HRESULT`-shaped `u32`s widened to `i32` (hence the large negatives);
/// only the three Moonlight names have a distinct meaning, everything else prints the raw code
/// since only the host's log has the cause.
pub fn gamestream_end_message(code: i32) -> String {
    /// `NVST_DISCONN_SERVER_TERMINATED_CLOSED` — the app exited, or the user stopped the session
    /// on the host.
    const GRACEFUL: i32 = 0x8003_0023_u32 as i32;
    /// `NVST_DISCONN_SERVER_VFP_PROTECTED_CONTENT`.
    const PROTECTED_CONTENT: i32 = 0x800e_9302_u32 as i32;
    /// `NVST_DISCONN_SERVER_VIDEO_ENCODER_CONVERT_INPUT_FRAME_FAILED`.
    const ENCODER_FAILED: i32 = 0x800e_9403_u32 as i32;

    match code {
        // 0 is also what a control stream that simply disconnected leaves behind, having sent no
        // termination packet at all — indistinguishable from a graceful close, and the same
        // sentence covers both.
        0 | GRACEFUL => "The host closed the connection.".into(),
        PROTECTED_CONTENT => "The host stopped streaming because something on screen is protected content — \
             DRM-protected video can't be captured."
            .into(),
        ENCODER_FAILED => "The host's video encoder failed — lower the resolution or frame rate, or check the \
             host's GPU driver."
            .into(),
        other => format!(
            "The host ended the session unexpectedly (code 0x{:08x}) — the host's own log has the \
             cause.",
            other as u32
        ),
    }
}

/// Extract `PunktfunkError` from anyhow chain for user-facing messages.
pub fn friendly(err: &anyhow::Error) -> String {
    err.downcast_ref::<PunktfunkError>()
        .map_or_else(|| format!("{err:#}"), connect_message)
}
