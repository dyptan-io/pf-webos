//! Side-effecting operations app-state logic asks the runtime to perform, instead of
//! calling `crate::services::*`/`crate::session::*` inline. See `docs/REFACTOR_PLAN.md`
//! §5 and `docs/HANDOFF_layered_refactor.md` for the current scope: only the clean,
//! fire-and-forget cases are routed through this enum so far (see handoff for the rest).
pub enum Effect {
    /// Toggle the on-screen log overlay (was a direct `crate::real::set_log_overlay_enabled` call).
    SetLogOverlay(bool),
}

/// Runs one effect. Lives outside `core::state` deliberately — this is the one place
/// allowed to reach into `crate::real`/`crate::services`/`crate::session` on the app
/// logic's behalf.
pub fn execute(effect: Effect) {
    match effect {
        Effect::SetLogOverlay(on) => crate::real::set_log_overlay_enabled(on),
    }
}
