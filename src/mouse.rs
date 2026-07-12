//! Maps SDL2 mouse events — the webOS Magic Remote's pointer mode surfaces as plain
//! SDL2 `MouseMotion`/`MouseButtonDown`/`MouseButtonUp`/`MouseWheel` events, same as
//! any desktop mouse — to punktfunk's wire `InputEvent`s, so pointing/clicking with
//! the remote drives the host's real cursor during a stream (`main.rs`'s streaming
//! loop), the same way aurora-tv/moonlight-tv forward their remote's pointer.
use punktfunk_core::input::{InputEvent, InputKind};
use sdl2::mouse::MouseButton;

/// GameStream's classic mouse-button numbering (1=left..5=X2) — the convention
/// `punktfunk-host`'s injectors expect in `MouseButtonDown`/`MouseButtonUp`'s `code`
/// (confirmed via `gs_button_to_evdev` in `punktfunk-host/src/inject.rs`).
fn button_code(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        MouseButton::X1 => Some(4),
        MouseButton::X2 => Some(5),
        MouseButton::Unknown => None,
    }
}

/// `None` for a button id the host has no mapping for (`MouseButton::Unknown`) —
/// the caller just drops the event.
pub fn button_event(button: MouseButton, pressed: bool) -> Option<InputEvent> {
    Some(InputEvent {
        kind: if pressed { InputKind::MouseButtonDown } else { InputKind::MouseButtonUp },
        _pad: [0; 3],
        code: button_code(button)?,
        x: 0,
        y: 0,
        flags: 0,
    })
}

/// Absolute pointer position — `client_w`/`client_h` is this app's own coordinate
/// space (the physical panel resolution the SDL2 window/mouse coordinates are in,
/// not necessarily the negotiated stream resolution); the host normalizes against
/// it before mapping into the output region (see `InputKind::MouseMoveAbs` docs) —
/// the same absolute-pointer path the pre-stream menu's hover/click already rides,
/// just forwarded to the host instead of used for local UI focus.
///
/// A previous attempt applied a fixed `SENSITIVITY` scale here (<1.0, centered on
/// the reported client size) to make the host cursor feel "slower." Symptom: the
/// cursor got stuck around the middle of the screen, never reaching anywhere near
/// the edges — much more restricted than the scale factor alone should produce.
/// Likely cause: the remote's own pointer already has *some* system-level gain
/// applied before these coordinates ever reach SDL2 (i.e. `x`/`y` may not actually
/// span the full `0..client_w`/`0..client_h` range even when physically pointing at
/// the panel's true edges) — layering an *additional* scale on top of an
/// already-restricted range compounds instead of just slowing things down. Reverted
/// to plain passthrough until the real range is confirmed; re-derive any
/// "sensitivity" adjustment from that, not from an assumed full-range span.
pub fn move_event(x: i32, y: i32, client_w: u32, client_h: u32) -> InputEvent {
    InputEvent {
        kind: InputKind::MouseMoveAbs,
        _pad: [0; 3],
        code: 0,
        x,
        y,
        flags: (client_w << 16) | (client_h & 0xffff),
    }
}

/// `code` distinguishes the scroll axis (`0` = vertical, `1` = horizontal — see
/// `punktfunk-host`'s `SCROLL_HORIZONTAL`); `delta` is the signed scroll amount,
/// SDL2's `MouseWheelEvent.y`/`.x` passed straight through.
pub fn scroll_event(delta: i32, horizontal: bool) -> InputEvent {
    InputEvent {
        kind: InputKind::MouseScroll,
        _pad: [0; 3],
        code: u32::from(horizontal),
        x: delta,
        y: 0,
        flags: 0,
    }
}
