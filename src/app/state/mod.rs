//! Per-screen app-state logic (event handling, state transitions). Split out of the
//! former fused `app/<screen>.rs` modules — see `docs/REFACTOR_PLAN.md` §5.
//! Rendering counterparts live in `app::view`.
mod about;
pub(crate) mod addhost;
pub(crate) mod diagnostics;
mod edithost;
mod experimental;
mod forget;
mod home;
pub(crate) mod hostmenu;
mod pairing;
mod pinlimit;
pub(crate) mod reach;
pub(crate) mod sendlogs;
mod settings;
pub(crate) mod speedtest;
mod wake;
mod wakesettings;
