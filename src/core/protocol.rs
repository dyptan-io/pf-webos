//! Which streaming protocol a host speaks, and what "we trust this host" means for it.
//!
//! Pure data — the behaviour behind each variant lives in `crate::backend`. Kept in `core` so
//! `KnownHost` can carry it without dragging any I/O into the domain layer.
use serde::{Deserialize, Serialize};

/// The protocol a host speaks. Set once, when the host is first learned, and never probed
/// again: mDNS service type decides it for discovered hosts, and manual IP entry resolves it
/// by trying punktfunk first (see `docs/GameStream-Plan.md`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Native punktfunk (`punktfunk_core`), the default — an older `known-hosts.json` has no
    /// `protocol` field at all, and every host in it is necessarily this.
    #[default]
    Punktfunk,
    /// `GameStream`: Sunshine and compatible forks. Gated by `Settings::gamestream_enabled`.
    GameStream,
}

/// Why we believe a host is the host we paired with. This is *not* protocol-neutral, which is
/// why it isn't just a fingerprint: punktfunk pins the host's self-signed leaf SHA-256 and
/// carries it into every mTLS call, whereas `GameStream` trusts a client cert pair and re-derives
/// pair state from `/serverinfo`'s `PairStatus` — there is no host key for us to pin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostTrust {
    /// Discovered but never paired.
    #[default]
    Unpaired,
    /// punktfunk: the host's pinned leaf certificate SHA-256.
    Pinned([u8; 32]),
    /// `GameStream`: our client cert is registered with the host. Nothing host-side to store —
    /// `/serverinfo` is the source of truth, so this can go stale and must be re-checked.
    ClientCertPaired,
}

impl HostTrust {
    /// The punktfunk pin, if this is a punktfunk trust. Every mTLS caller
    /// (`services::library`, `services::art`, `session::connect`) needs exactly this.
    pub fn pin(self) -> Option<[u8; 32]> {
        match self {
            Self::Pinned(fp) => Some(fp),
            Self::Unpaired | Self::ClientCertPaired => None,
        }
    }

    pub fn is_paired(self) -> bool {
        !matches!(self, Self::Unpaired)
    }
}
