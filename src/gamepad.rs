//! Maps SDL2 `GameController` events to punktfunk's wire `InputEvent`s. Per-transition
//! events (`GamepadButton`/`GamepadAxis`), not the `GamepadState` snapshot form — the
//! snapshot is only sent to hosts that advertise support for it, and we don't yet track
//! that host capability flag here, so per-transition is the universally-compatible
//! choice (see `punktfunk_core::input` docs).
use punktfunk_core::input::{gamepad, InputEvent, InputKind};
use sdl2::controller::{Axis, Button};

/// SDL2's `Button` enum (exhaustively matched — all 20 current variants) → punktfunk's
/// `BTN_*` wire bit.
fn button_bit(button: Button) -> u32 {
    match button {
        Button::A => gamepad::BTN_A,
        Button::B => gamepad::BTN_B,
        Button::X => gamepad::BTN_X,
        Button::Y => gamepad::BTN_Y,
        Button::Back => gamepad::BTN_BACK,
        Button::Guide => gamepad::BTN_GUIDE,
        Button::Start => gamepad::BTN_START,
        Button::LeftStick => gamepad::BTN_LS_CLICK,
        Button::RightStick => gamepad::BTN_RS_CLICK,
        Button::LeftShoulder => gamepad::BTN_LB,
        Button::RightShoulder => gamepad::BTN_RB,
        Button::DPadUp => gamepad::BTN_DPAD_UP,
        Button::DPadDown => gamepad::BTN_DPAD_DOWN,
        Button::DPadLeft => gamepad::BTN_DPAD_LEFT,
        Button::DPadRight => gamepad::BTN_DPAD_RIGHT,
        Button::Misc1 => gamepad::BTN_MISC1,
        Button::Paddle1 => gamepad::BTN_PADDLE1,
        Button::Paddle2 => gamepad::BTN_PADDLE2,
        Button::Paddle3 => gamepad::BTN_PADDLE3,
        Button::Paddle4 => gamepad::BTN_PADDLE4,
        Button::Touchpad => gamepad::BTN_TOUCHPAD,
    }
}

/// SDL2's `Axis` enum → punktfunk's `AXIS_*` wire id.
fn axis_id(axis: Axis) -> u32 {
    match axis {
        Axis::LeftX => gamepad::AXIS_LS_X,
        Axis::LeftY => gamepad::AXIS_LS_Y,
        Axis::RightX => gamepad::AXIS_RS_X,
        Axis::RightY => gamepad::AXIS_RS_Y,
        Axis::TriggerLeft => gamepad::AXIS_LT,
        Axis::TriggerRight => gamepad::AXIS_RT,
    }
}

/// `pad` is the wire pad index (`flags`) — 0 for the single-controller case this phase
/// targets (multi-pad indexing is a follow-up once one controller round-trips cleanly).
pub fn button_event(button: Button, pressed: bool, pad: u8) -> InputEvent {
    InputEvent {
        kind: InputKind::GamepadButton,
        _pad: [0; 3],
        code: button_bit(button),
        x: if pressed { 1 } else { 0 },
        y: 0,
        flags: u32::from(pad),
    }
}

/// SDL2 sticks are already i16 (−32768..32767) matching the wire's range, so X passes
/// straight through. Y does not: confirmed on-device (`DualSense` over Bluetooth, this
/// webOS/Linux SDL2 build) that pushing a stick up/forward reports a *negative* raw
/// value — the opposite of the wire's XInput/Moonlight "+y = up" convention — so both
/// sticks' Y axes are negated before sending (`saturating_neg` since raw `i16::MIN`
/// has no positive counterpart in range). Triggers arrive as SDL2's 0..32767 range —
/// punktfunk wants 0..255, so those are rescaled.
pub fn axis_event(axis: Axis, value: i16, pad: u8) -> InputEvent {
    let scaled = match axis {
        Axis::TriggerLeft | Axis::TriggerRight => (i32::from(value) * 255) / 32767,
        Axis::LeftY | Axis::RightY => i32::from(value.saturating_neg()),
        _ => i32::from(value),
    };
    InputEvent {
        kind: InputKind::GamepadAxis,
        _pad: [0; 3],
        code: axis_id(axis),
        x: scaled,
        y: 0,
        flags: u32::from(pad),
    }
}
