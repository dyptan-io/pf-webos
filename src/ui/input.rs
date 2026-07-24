//! Raw SDL2 keyboard/gamepad input mapped to debounced `MenuEvent`s.
//!
//! Split out of the former single-file `ui.rs`; see `super`'s module docs.


/// A menu event, already debounced from the raw SDL2 input (keyboard arrows — which
/// the webOS Magic Remote's d-pad mode surfaces as — and gamepad d-pad both map here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    Up,
    Down,
    Left,
    Right,
    Confirm,
    Back,
    /// "Forget this host" on the sidebar — deliberately a separate key from
    /// Back/Confirm so it can't be hit by accident (see `app.rs`).
    Secondary,
}

/// The raw SDL keycode the LG Magic Remote's physical Back button delivers on this TV —
/// identified via on-device input logging. It is NOT Escape/Backspace/AcBack and has no
/// named rust-sdl2 `Keycode` variant, so it must be matched by raw value. This is the
/// only usable hardware Back the remote gives the app: the Home button instead SIGTERMs
/// the process (webOS closes the app), so it can't act as an in-app "back". See
/// `docs/NOTES.md`'s note on Back never arriving as a scancode — it arrives as this
/// keycode via the event API instead.
pub const WEBOS_BACK_KEYCODE: i32 = 2_097_155;

pub fn menu_event_for_key(keycode: sdl2::keyboard::Keycode) -> Option<MenuEvent> {
    use sdl2::keyboard::Keycode;
    Some(match keycode {
        Keycode::Up => MenuEvent::Up,
        Keycode::Down => MenuEvent::Down,
        Keycode::Left => MenuEvent::Left,
        Keycode::Right => MenuEvent::Right,
        Keycode::Return | Keycode::Return2 | Keycode::KpEnter => MenuEvent::Confirm,
        // AcBack: some remotes' dedicated Back button sends the browser-style "AC
        // Back" key rather than Escape/Backspace — map all three so Back works
        // regardless of which one this remote actually sends.
        Keycode::Backspace | Keycode::Escape | Keycode::AcBack => MenuEvent::Back,
        Keycode::Delete => MenuEvent::Secondary,
        // The Magic Remote's Back button (see `WEBOS_BACK_KEYCODE`).
        k if k.into_i32() == WEBOS_BACK_KEYCODE => MenuEvent::Back,
        _ => return None,
    })
}

pub fn menu_event_for_button(button: sdl2::controller::Button) -> Option<MenuEvent> {
    use sdl2::controller::Button;
    Some(match button {
        Button::DPadUp => MenuEvent::Up,
        Button::DPadDown => MenuEvent::Down,
        Button::DPadLeft => MenuEvent::Left,
        Button::DPadRight => MenuEvent::Right,
        Button::A => MenuEvent::Confirm,
        // `Back` (SDL's dedicated back/select button) in addition to `B`: on this TV
        // the Magic Remote surfaces as a game controller ("Smart Remote RCU Input"),
        // and its physical Back button does *not* arrive as `B` — the temporary input
        // logging in `main.rs` is what pins down which button it actually is. Mapping
        // `Back` here is the low-risk best guess (no game relies on Select as a 1.5s
        // hold, which is all it can trigger in-stream); widen this once the log says.
        Button::B | Button::Back => MenuEvent::Back,
        Button::Y => MenuEvent::Secondary,
        _ => return None,
    })
}

/// Left-stick tilt past this fraction of full deflection (of i16's ±32768) counts as
/// a directional press — well past center-rest noise.
pub const STICK_MENU_DEADZONE: i16 = 16_000;

/// Edge-detects the left stick's X/Y axes into `MenuEvent`s, per-axis, so a hold
/// fires once on crossing the deadzone and doesn't repeat until the stick passes back
/// through center — the same one-shot-per-press behavior a D-pad button already has
/// (SDL2 doesn't auto-repeat `ControllerButtonDown` while held).
#[derive(Default)]
pub struct StickMenuNav {
    x: Option<MenuEvent>,
    y: Option<MenuEvent>,
}

impl StickMenuNav {
    pub fn axis_event(&mut self, axis: sdl2::controller::Axis, value: i16) -> Option<MenuEvent> {
        use sdl2::controller::Axis;
        match axis {
            Axis::LeftX => Self::edge(&mut self.x, value, MenuEvent::Left, MenuEvent::Right),
            // Negative is up (see `gamepad.rs`'s `axis_event` docs for why).
            Axis::LeftY => Self::edge(&mut self.y, value, MenuEvent::Up, MenuEvent::Down),
            _ => None,
        }
    }

    fn edge(state: &mut Option<MenuEvent>, value: i16, neg: MenuEvent, pos: MenuEvent) -> Option<MenuEvent> {
        let dir = if value <= -STICK_MENU_DEADZONE {
            Some(neg)
        } else if value >= STICK_MENU_DEADZONE {
            Some(pos)
        } else {
            None
        };
        if dir == *state {
            return None;
        }
        *state = dir;
        dir
    }
}

/// The Magic Remote's number buttons (0-9) surface as plain keyboard digit keys —
/// used for direct PIN entry (type a digit, auto-advance) instead of cycling each
/// digit with left/right.
pub fn digit_key_value(keycode: sdl2::keyboard::Keycode) -> Option<u8> {
    use sdl2::keyboard::Keycode;
    Some(match keycode {
        Keycode::Num0 | Keycode::Kp0 => 0,
        Keycode::Num1 | Keycode::Kp1 => 1,
        Keycode::Num2 | Keycode::Kp2 => 2,
        Keycode::Num3 | Keycode::Kp3 => 3,
        Keycode::Num4 | Keycode::Kp4 => 4,
        Keycode::Num5 | Keycode::Kp5 => 5,
        Keycode::Num6 | Keycode::Kp6 => 6,
        Keycode::Num7 | Keycode::Kp7 => 7,
        Keycode::Num8 | Keycode::Kp8 => 8,
        Keycode::Num9 | Keycode::Kp9 => 9,
        _ => return None,
    })
}

