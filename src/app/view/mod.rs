//! Per-screen draw-command building (geometry + render calls). Split out of the
//! former fused `app/<screen>.rs` modules — see `docs/REFACTOR_PLAN.md` §5.
//! Logic counterparts live in `app::state`.
mod about;
mod addhost;
mod diagnostics;
mod edithost;
mod experimental;
mod forget;
mod home;
mod hostmenu;
mod pairing;
mod pinlimit;
mod sendlogs;
mod settings;
mod speedtest;
mod wake;
mod wakesettings;
