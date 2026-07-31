//! Raw SDL2 keyboard/gamepad input mapped to debounced `MenuEvent`s.

use crate::core::event::MenuEvent;

/// Magic Remote Back button keycode (not Escape/Backspace/AcBack; identified via logs).
/// Only usable hardware Back; Home button SIGTERMs the app.
pub const WEBOS_BACK_KEYCODE: i32 = 2_097_155;

pub fn menu_event_for_key(keycode: sdl2::keyboard::Keycode) -> Option<MenuEvent> {
    use sdl2::keyboard::Keycode;
    Some(match keycode {
        Keycode::Up => MenuEvent::Up,
        Keycode::Down => MenuEvent::Down,
        Keycode::Left => MenuEvent::Left,
        Keycode::Right => MenuEvent::Right,
        Keycode::Return | Keycode::Return2 | Keycode::KpEnter => MenuEvent::Confirm,
        // Map Backspace/Escape/AcBack so Back works with any remote variant.
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
        // WHY: Magic Remote's Back doesn't arrive as B; Back is low-risk guess.
        Button::B | Button::Back => MenuEvent::Back,
        Button::Y => MenuEvent::Secondary,
        _ => return None,
    })
}

/// Stick deflection threshold for directional press (well past center noise).
pub const STICK_MENU_DEADZONE: i16 = 16_000;

/// Edge-detect left stick X/Y to `MenuEvents` (one-shot per cross, repeats on re-center).
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

/// webOS Magic Remote scancodes — outside rust-sdl2's enum, needs raw polling.
/// `SDL_SCANCODE_WEBOS_{RED,GREEN,YELLOW,BLUE} = 486..489` in `webosbrew/SDL-webOS`'s `SDL_scancode.h`.
/// Red (486) never reaches the app on-device (OS-intercepted), so Blue drives the pacing toggle.
pub const WEBOS_GREEN_SCANCODE: i32 = 487;
pub const WEBOS_YELLOW_SCANCODE: i32 = 488;
pub const WEBOS_BLUE_SCANCODE: i32 = 489;

/// webOS EXIT key (`SDL_SCANCODE_WEBOS_EXIT`). A held/root-level Back is turned
/// by the OS into its own EXIT gesture and delivered as this discrete keypress —
/// *not* as a held [`WEBOS_BACK_KEYCODE`] — so it's the reliable signal for
/// "open the disconnect/quit dialog" (a short Back tap still arrives as Back).
/// Needs `SDL_WEBOS_ACCESS_POLICY_KEYS_EXIT` set before window creation to reach
/// the app instead of `SIGTERM`ing it. See `docs/NOTES.md`.
pub const WEBOS_EXIT_SCANCODE: i32 = 505;

/// Check a Magic Remote button via raw SDL keyboard state (safe after `sdl2::init`).
pub fn webos_scancode_down(scancode: i32) -> bool {
    unsafe {
        let mut count = 0;
        let state = sdl2::sys::SDL_GetKeyboardState(&mut count);
        !state.is_null() && scancode < count && *state.offset(scancode as isize) != 0
    }
}

/// Extract digit from Magic Remote number buttons (0-9 direct PIN entry).
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
