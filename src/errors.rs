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

/// Extract `PunktfunkError` from anyhow chain for user-facing messages.
pub fn friendly(err: &anyhow::Error) -> String {
    err.downcast_ref::<PunktfunkError>()
        .map_or_else(|| format!("{err:#}"), connect_message)
}
