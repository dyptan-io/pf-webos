//! Client-side Wake-on-LAN: parse stored MAC strings and hand them to the shared
//! `punktfunk_core::wol` magic-packet sender — the same core module the other
//! (linux/windows/android) clients wrap, kept as its own tiny module here rather than
//! inlined into `app.rs` so the network bit stays independently testable/greppable.
//! A sleeping host has no ARP entry, so the broadcast the core builds (each NIC's
//! subnet-directed broadcast, plus the limited broadcast) is what actually reaches it;
//! `last_ip`, when known, is additionally unicast.
use std::io::Write as _;
use std::net::Ipv4Addr;

/// Sends a magic packet to every parseable MAC in `macs`. Returns whether at least one
/// packet actually went out — `false` means either no MAC was on record for this host
/// (nothing to wake) or the send itself failed (no usable network interface), either of
/// which the caller should treat as "couldn't wake it".
pub fn wake(macs: &[String], last_ip: Option<Ipv4Addr>) -> bool {
    let parsed: Vec<[u8; 6]> = macs.iter().filter_map(|s| punktfunk_core::wol::parse_mac(s)).collect();
    if parsed.is_empty() {
        return false;
    }
    punktfunk_core::wol::send_magic_packet(&parsed, last_ip).is_ok()
}

/// `wake`, plus a log line recording the outcome — shared by `app.rs`'s explicit "Send"
/// action and its periodic resend while a wake is in flight, so neither has to spell out
/// the log message itself. `name` is just for a readable log line (the host's display
/// name), not part of the wake mechanics.
pub fn wake_and_log(macs: &[String], last_ip: Option<Ipv4Addr>, name: &str, log: &mut std::fs::File) -> bool {
    let ok = wake(macs, last_ip);
    let _ = writeln!(log, "wake-on-lan: sent to {name} ({} mac(s)), ok={ok}", macs.len());
    ok
}
