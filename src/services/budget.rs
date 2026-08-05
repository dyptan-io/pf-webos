//! The connect-path time budgets, in one place so "how long do we wait for a host" doesn't mean
//! two different things depending on which protocol is in front of it.
//!
//! Lives in `services` rather than `backend` because the `GameStream` HTTP client needs it and that
//! file is also compiled standalone into `src/bin/gsprobe.rs`, which has no `backend` module — see
//! `backend::gamestream::query`'s note on that isolation.
use std::time::Duration;

/// One handshake attempt against a host we already trust: it is either reachable now or it is off,
/// and a long wait on a black launch scrim buys nothing. Also the per-request TCP connect budget.
pub const HANDSHAKE: Duration = Duration::from_secs(5);

/// A host that answered but isn't ready to stream yet: punktfunk's park-until-approved TOFU
/// connection, `GameStream`'s PIN being typed into the host's web UI, and — the case this exists
/// for — `/launch` on a freshly booted host, where Sunshine can spend tens of seconds bringing up
/// the display and the app before it will accept a session. A shorter budget there sent the user
/// back to the menu with "couldn't connect" against a host that was merely still starting.
pub const HOST_WAIT: Duration = Duration::from_secs(185);

/// The ambient reachability dot's per-host budget, for every protocol. Short on purpose: an
/// unreachable host on a LAN fails fast (no route / refused), and one slow enough to miss this is
/// not meaningfully "available". Not [`HANDSHAKE`] — nobody is waiting on this answer, so it is
/// allowed to be wrong about a sluggish host rather than hold the sweep open.
pub const PROBE: Duration = Duration::from_secs(2);

/// One host request that should already have an answer: a library listing, a `/launch`, a
/// `/serverinfo`. Not a wait for the host to become ready — that is [`HOST_WAIT`], spent by re-trying
/// requests with this budget rather than by stretching one of them.
pub const REQUEST: Duration = Duration::from_secs(10);

/// Connect budget for the speed test's throwaway session. Longer than [`HANDSHAKE`] because the
/// host brings up a real encode session for it, and the user opened this screen expecting to wait —
/// but not [`HOST_WAIT`]: a host that needs minutes here has already answered the question.
pub const SPEED_TEST: Duration = Duration::from_secs(20);
