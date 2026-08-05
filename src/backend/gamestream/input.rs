//! This client's internal [`InputEvent`] → `GameStream`'s [`ClientInputEvent`].
//!
//! The two wire formats already agree on almost everything, because punktfunk's input plane was
//! modelled on `GameStream`'s: buttons are the same `buttonFlags` bit positions, sticks are
//! −32768..32767 with +y up, triggers 0..255, mouse buttons are 1=left..5=X2, and scroll is in
//! `WHEEL_DELTA(120)` units. So this is a re-packing, not a re-mapping — the two real differences
//! are:
//!
//! * **Keyboard** — the wire keycode is `0x8000 | VK`. Our events carry the bare US-positional VK
//!   (`keyboard::key_event`), and modifiers are a separate field there rather than packed into the
//!   event, so they are tracked here from the key stream itself.
//! * **Gamepad** — `GameStream` has no per-transition button/axis event: every change sends the
//!   pad's *whole* state. So state is accumulated per pad and re-emitted on each change, and a pad
//!   is announced with a `ControllerConnect` before its first state.

use moonlight_common::stream::control::{
    ControllerButtons, ControllerCapabilities, ControllerType, KeyAction, KeyCode, KeyFlags, KeyModifiers, MouseButton,
    MouseButtonAction,
};
use moonlight_common::stream::proto::control::input_batcher::ClientInputEvent;
use punktfunk_core::input::{gamepad, InputEvent, InputKind};

use crate::services::store::GamepadType;

/// Pads this translator tracks. The client drives pad 0 only (see `session::connect`'s note on
/// the session-default pad kind), but the wire carries an index and a host that mixed them up
/// would be much harder to diagnose than one unused array slot.
const PADS: usize = 4;

/// High byte `GameStream` expects on a keycode; the low byte is the VK.
const KEY_PREFIX: u16 = 0x8000;

/// The accumulated state of one pad, as `GameStream` wants it: whole-pad, every time.
#[derive(Clone, Copy)]
struct PadState {
    /// Whether a `ControllerConnect` has been sent for this pad.
    announced: bool,
    buttons: ControllerButtons,
    /// 0..255, the wire convention on both sides.
    left_trigger: u8,
    right_trigger: u8,
    /// −32768..32767, +y up.
    ls_x: i16,
    ls_y: i16,
    rs_x: i16,
    rs_y: i16,
}

/// Hand-written because `ControllerButtons` (a `bitflags` type) has no `Default`.
impl Default for PadState {
    fn default() -> Self {
        Self {
            announced: false,
            buttons: ControllerButtons::empty(),
            left_trigger: 0,
            right_trigger: 0,
            ls_x: 0,
            ls_y: 0,
            rs_x: 0,
            rs_y: 0,
        }
    }
}

impl PadState {
    fn state_event(&self, pad: u8) -> ClientInputEvent {
        ClientInputEvent::ControllerState {
            controller_number: pad,
            pressed_buttons: self.buttons,
            left_trigger: f32::from(self.left_trigger) / f32::from(u8::MAX),
            right_trigger: f32::from(self.right_trigger) / f32::from(u8::MAX),
            left_stick_x: norm(self.ls_x),
            left_stick_y: norm(self.ls_y),
            right_stick_x: norm(self.rs_x),
            right_stick_y: norm(self.rs_y),
        }
    }
}

/// i16 stick value → the −1..1 the crate's packet builder takes. `i16::MIN` maps slightly past
/// −1; the builder clamps, so full deflection stays full deflection.
fn norm(v: i16) -> f32 {
    f32::from(v) / f32::from(i16::MAX)
}

pub struct InputTranslator {
    pads: [PadState; PADS],
    /// Live modifier set, folded from the key stream — see the module docs.
    modifiers: KeyModifiers,
    /// What kind of pad the host is told this session's controllers are.
    controller_type: ControllerType,
}

impl InputTranslator {
    pub fn new(gamepad_type: GamepadType) -> Self {
        Self {
            pads: [PadState::default(); PADS],
            modifiers: KeyModifiers::empty(),
            controller_type: controller_type_for(gamepad_type),
        }
    }

    /// Appends the `GameStream` events equivalent to `ev` — usually one, two when a pad has to be
    /// announced first, none for an event this protocol has no equivalent for.
    ///
    /// Takes the output buffer rather than returning a collection: this runs per input event, at
    /// mouse report rates, and the caller reuses one buffer for the session.
    pub fn translate(&mut self, ev: &InputEvent, out: &mut Vec<ClientInputEvent>) {
        match ev.kind {
            InputKind::KeyDown | InputKind::KeyUp => {
                let down = ev.kind == InputKind::KeyDown;
                let Ok(vk) = u8::try_from(ev.code) else {
                    return;
                };
                self.set_modifier(vk, down);
                out.push(ClientInputEvent::Keyboard {
                    action: if down { KeyAction::Down } else { KeyAction::Up },
                    flags: KeyFlags::empty(),
                    key_code: KeyCode((KEY_PREFIX | u16::from(vk)) as i16),
                    modifiers: self.modifiers,
                });
            }
            InputKind::MouseMove => out.push(ClientInputEvent::MouseMoveRelative {
                delta_x: clamp_i16(ev.x),
                delta_y: clamp_i16(ev.y),
            }),
            InputKind::MouseMoveAbs => {
                // `flags` packs the client's own coordinate space as `(w << 16) | h`; the host
                // normalizes against it. Zero is documented as "drop the event" on our own wire
                // and would trip the crate's debug assertion on the reference size.
                let (w, h) = ((ev.flags >> 16) as u16, (ev.flags & 0xffff) as u16);
                if w == 0 || h == 0 {
                    return;
                }
                out.push(ClientInputEvent::MouseMoveAbsolute {
                    x: clamp_i16(ev.x),
                    y: clamp_i16(ev.y),
                    reference_width: clamp_i16(i32::from(w)),
                    reference_height: clamp_i16(i32::from(h)),
                });
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                let Some(button) = mouse_button(ev.code) else {
                    return;
                };
                out.push(ClientInputEvent::MouseButton {
                    action: if ev.kind == InputKind::MouseButtonDown {
                        MouseButtonAction::Press
                    } else {
                        MouseButtonAction::Release
                    },
                    button,
                });
            }
            // `code` is the axis: 0 vertical, 1 horizontal (see `mouse::ScrollAccumulator`).
            InputKind::MouseScroll => out.push(if ev.code == 1 {
                ClientInputEvent::MouseScrollHorizontal {
                    scroll_x: clamp_i16(ev.x),
                }
            } else {
                ClientInputEvent::MouseScrollVertical {
                    scroll_y: clamp_i16(ev.x),
                }
            }),
            InputKind::GamepadButton => {
                let Some(pad) = self.pad_index(ev.flags) else {
                    return;
                };
                // Only the bits `GameStream` has; our extended set (paddles, touchpad, misc)
                // occupies the same positions, and `from_bits_truncate` drops what it doesn't
                // know rather than refusing the whole mask.
                let bit = ControllerButtons::from_bits_truncate(ev.code);
                self.announce(pad, out);
                let state = &mut self.pads[pad as usize];
                state.buttons.set(bit, ev.x != 0);
                out.push(state.state_event(pad));
            }
            InputKind::GamepadAxis => {
                let Some(pad) = self.pad_index(ev.flags) else {
                    return;
                };
                self.announce(pad, out);
                let state = &mut self.pads[pad as usize];
                match ev.code {
                    gamepad::AXIS_LS_X => state.ls_x = clamp_i16(ev.x),
                    gamepad::AXIS_LS_Y => state.ls_y = clamp_i16(ev.x),
                    gamepad::AXIS_RS_X => state.rs_x = clamp_i16(ev.x),
                    gamepad::AXIS_RS_Y => state.rs_y = clamp_i16(ev.x),
                    gamepad::AXIS_LT => state.left_trigger = clamp_u8(ev.x),
                    gamepad::AXIS_RT => state.right_trigger = clamp_u8(ev.x),
                    _ => return,
                }
                out.push(state.state_event(pad));
            }
            InputKind::GamepadRemove => {
                let Some(pad) = self.pad_index(ev.flags) else {
                    return;
                };
                self.pads[pad as usize] = PadState::default();
                out.push(ClientInputEvent::ControllerDisconnect { controller_number: pad });
            }
            // `GamepadState`/`GamepadArrival` are punktfunk's idempotent snapshot plane, which
            // this client only sends to a host that advertised support for it — never to a
            // `GameStream` host, whose whole-state packet this translator builds instead. Touch
            // and committed text have no route on this protocol from a TV client: there is no
            // touchscreen and no IME in the stream.
            InputKind::GamepadState
            | InputKind::GamepadArrival
            | InputKind::TouchDown
            | InputKind::TouchMove
            | InputKind::TouchUp
            | InputKind::TextInput => {}
        }
    }

    /// Pad index if this event addresses one we track. A higher index is dropped rather than
    /// folded into pad 0 — silently attributing input to the wrong pad is worse than losing it.
    fn pad_index(&self, flags: u32) -> Option<u8> {
        let pad = flags & 0xff;
        (pad < PADS as u32).then_some(pad as u8)
    }

    /// Sends the pad's `ControllerConnect` the first time it is touched. The host builds its
    /// virtual device from this, so it must precede the pad's first state packet.
    fn announce(&mut self, pad: u8, out: &mut Vec<ClientInputEvent>) {
        if std::mem::replace(&mut self.pads[pad as usize].announced, true) {
            return;
        }
        out.push(ClientInputEvent::ControllerConnect {
            controller_number: pad,
            ty: self.controller_type,
            // Analog triggers and rumble are what this client actually drives: rumble goes to
            // SDL's evdev force feedback (`session::pump_feedback_once`). Motion, touchpad,
            // battery and LED are deliberately absent — claiming them would have the host ask
            // for reports nothing here produces.
            capabilities: ControllerCapabilities::ANALOG_TRIGGERS | ControllerCapabilities::RUMBLE,
            supported_buttons: ControllerButtons::all(),
        });
    }

    /// Folds a VK press/release into the modifier field. Both the generic
    /// (`VK_SHIFT`) and the sided (`VK_LSHIFT`/`VK_RSHIFT`) codes count: which one arrives
    /// depends on the key, and the host only wants to know the modifier is down.
    fn set_modifier(&mut self, vk: u8, down: bool) {
        let modifier = match vk {
            0x10 | 0xA0 | 0xA1 => KeyModifiers::SHIFT,
            0x11 | 0xA2 | 0xA3 => KeyModifiers::CTRL,
            0x12 | 0xA4 | 0xA5 => KeyModifiers::ALT,
            0x5B | 0x5C => KeyModifiers::META,
            _ => return,
        };
        self.modifiers.set(modifier, down);
    }
}

/// The pad kind announced to the host. `Automatic` is already resolved against the attached
/// controller by `runtime::resolve_gamepad_type` before a session starts, so an `Auto` here means
/// nothing was attached — `Unknown` then lets the host pick, exactly as our own handshake default
/// does.
fn controller_type_for(gamepad_type: GamepadType) -> ControllerType {
    match gamepad_type {
        GamepadType::Auto => ControllerType::Unknown,
        GamepadType::XboxOne => ControllerType::Xbox,
        GamepadType::DualSense | GamepadType::DualSenseEdge | GamepadType::DualShock4 => ControllerType::PlayStation,
        GamepadType::SwitchPro => ControllerType::Nintendo,
    }
}

/// `GameStream`'s classic 1=left..5=X2 numbering, which is also what our own events carry (see
/// `mouse::button_code`) — so this only validates the range.
fn mouse_button(code: u32) -> Option<MouseButton> {
    Some(match code {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        4 => MouseButton::X1,
        5 => MouseButton::X2,
        _ => return None,
    })
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, i32::from(u8::MAX)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: InputKind, code: u32, x: i32, flags: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags,
        }
    }

    /// The prefix is the one difference from our own keycodes, and getting it wrong is invisible
    /// until a host ignores every key.
    #[test]
    fn key_carries_the_gamestream_prefix() {
        let mut t = InputTranslator::new(GamepadType::XboxOne);
        let mut out = Vec::new();
        // VK_A.
        t.translate(&ev(InputKind::KeyDown, 0x41, 0, 0), &mut out);
        match out.as_slice() {
            [ClientInputEvent::Keyboard { key_code, .. }] => assert_eq!(key_code.0 as u16, 0x8041),
            other => panic!("expected one keyboard event, got {other:?}"),
        }
    }

    /// A pad must be announced before its first state, and only once.
    #[test]
    fn first_pad_event_announces_then_states() {
        let mut t = InputTranslator::new(GamepadType::XboxOne);
        let mut out = Vec::new();
        t.translate(&ev(InputKind::GamepadButton, gamepad::BTN_A, 1, 0), &mut out);
        assert!(matches!(
            out.as_slice(),
            [
                ClientInputEvent::ControllerConnect { .. },
                ClientInputEvent::ControllerState { .. }
            ]
        ));
        out.clear();
        t.translate(&ev(InputKind::GamepadButton, gamepad::BTN_B, 1, 0), &mut out);
        assert!(matches!(out.as_slice(), [ClientInputEvent::ControllerState { .. }]));
    }

    /// Buttons accumulate: `GameStream` gets whole-pad state, so a second press must not clear
    /// the first.
    #[test]
    fn buttons_accumulate_across_events() {
        let mut t = InputTranslator::new(GamepadType::XboxOne);
        let mut out = Vec::new();
        t.translate(&ev(InputKind::GamepadButton, gamepad::BTN_A, 1, 0), &mut out);
        out.clear();
        t.translate(&ev(InputKind::GamepadButton, gamepad::BTN_B, 1, 0), &mut out);
        match out.as_slice() {
            [ClientInputEvent::ControllerState { pressed_buttons, .. }] => {
                assert!(pressed_buttons.contains(ControllerButtons::A));
                assert!(pressed_buttons.contains(ControllerButtons::B));
            }
            other => panic!("expected one state event, got {other:?}"),
        }
    }
}
