//! Maps SDL2 keyboard events to punktfunk's wire `InputEvent`s. The webOS Magic
//! Remote's 5-way pad surfaces as plain SDL2 `Keycode::Up/Down/Left/Right` (same as
//! any desktop arrow key), forwarded to the host as real key presses during a stream
//! so directional navigation drives the host UI instead of only the local menu (see
//! `ui::menu_event_for_key`, used pre-stream for the same keycodes). USB keyboards
//! connected to the TV are handled the same way via SDL2 scancodes (physical key
//! positions) so the host receives QWERTY-positional VKs regardless of layout.
use punktfunk_core::input::{InputEvent, InputKind};
use sdl2::keyboard::Scancode;

/// SDL2 scancode → Windows VK code (`vk_to_evdev` on the host side).
/// `None` for keys not in that table.
fn vk_code(sc: Scancode) -> Option<u32> {
    Some(match sc {
        // ── Navigation / editing / whitespace ────────────────────────────────
        Scancode::Backspace   => 0x08, // VK_BACK
        Scancode::Tab         => 0x09, // VK_TAB
        Scancode::Return | Scancode::KpEnter => 0x0D, // VK_RETURN (numpad enter too)
        Scancode::Pause       => 0x13, // VK_PAUSE
        Scancode::CapsLock    => 0x14, // VK_CAPITAL
        Scancode::Escape      => 0x1B, // VK_ESCAPE
        Scancode::Space       => 0x20, // VK_SPACE
        Scancode::PageUp      => 0x21, // VK_PRIOR
        Scancode::PageDown    => 0x22, // VK_NEXT
        Scancode::End         => 0x23, // VK_END
        Scancode::Home        => 0x24, // VK_HOME
        Scancode::Left        => 0x25, // VK_LEFT
        Scancode::Up          => 0x26, // VK_UP
        Scancode::Right       => 0x27, // VK_RIGHT
        Scancode::Down        => 0x28, // VK_DOWN
        Scancode::PrintScreen => 0x2C, // VK_SNAPSHOT
        Scancode::Insert      => 0x2D, // VK_INSERT
        Scancode::Delete      => 0x2E, // VK_DELETE

        // ── Digit row ─────────────────────────────────────────────────────────
        Scancode::Num0 => 0x30, // VK_0
        Scancode::Num1 => 0x31, // VK_1
        Scancode::Num2 => 0x32, // VK_2
        Scancode::Num3 => 0x33, // VK_3
        Scancode::Num4 => 0x34, // VK_4
        Scancode::Num5 => 0x35, // VK_5
        Scancode::Num6 => 0x36, // VK_6
        Scancode::Num7 => 0x37, // VK_7
        Scancode::Num8 => 0x38, // VK_8
        Scancode::Num9 => 0x39, // VK_9

        // ── Letters A–Z (QWERTY positional) ──────────────────────────────────
        Scancode::A => 0x41,
        Scancode::B => 0x42,
        Scancode::C => 0x43,
        Scancode::D => 0x44,
        Scancode::E => 0x45,
        Scancode::F => 0x46,
        Scancode::G => 0x47,
        Scancode::H => 0x48,
        Scancode::I => 0x49,
        Scancode::J => 0x4A,
        Scancode::K => 0x4B,
        Scancode::L => 0x4C,
        Scancode::M => 0x4D,
        Scancode::N => 0x4E,
        Scancode::O => 0x4F,
        Scancode::P => 0x50,
        Scancode::Q => 0x51,
        Scancode::R => 0x52,
        Scancode::S => 0x53,
        Scancode::T => 0x54,
        Scancode::U => 0x55,
        Scancode::V => 0x56,
        Scancode::W => 0x57,
        Scancode::X => 0x58,
        Scancode::Y => 0x59,
        Scancode::Z => 0x5A,

        // ── Meta / context-menu ───────────────────────────────────────────────
        Scancode::LGui       => 0x5B, // VK_LWIN
        Scancode::RGui       => 0x5C, // VK_RWIN
        Scancode::Application => 0x5D, // VK_APPS

        // ── Numpad ────────────────────────────────────────────────────────────
        Scancode::Kp0        => 0x60, // VK_NUMPAD0
        Scancode::Kp1        => 0x61, // VK_NUMPAD1
        Scancode::Kp2        => 0x62, // VK_NUMPAD2
        Scancode::Kp3        => 0x63, // VK_NUMPAD3
        Scancode::Kp4        => 0x64, // VK_NUMPAD4
        Scancode::Kp5        => 0x65, // VK_NUMPAD5
        Scancode::Kp6        => 0x66, // VK_NUMPAD6
        Scancode::Kp7        => 0x67, // VK_NUMPAD7
        Scancode::Kp8        => 0x68, // VK_NUMPAD8
        Scancode::Kp9        => 0x69, // VK_NUMPAD9
        Scancode::KpMultiply => 0x6A, // VK_MULTIPLY
        Scancode::KpPlus     => 0x6B, // VK_ADD
        Scancode::KpMinus    => 0x6D, // VK_SUBTRACT
        Scancode::KpPeriod   => 0x6E, // VK_DECIMAL
        Scancode::KpDivide   => 0x6F, // VK_DIVIDE

        // ── Function keys ─────────────────────────────────────────────────────
        Scancode::F1  => 0x70,
        Scancode::F2  => 0x71,
        Scancode::F3  => 0x72,
        Scancode::F4  => 0x73,
        Scancode::F5  => 0x74,
        Scancode::F6  => 0x75,
        Scancode::F7  => 0x76,
        Scancode::F8  => 0x77,
        Scancode::F9  => 0x78,
        Scancode::F10 => 0x79,
        Scancode::F11 => 0x7A,
        Scancode::F12 => 0x7B,

        // ── Lock keys ─────────────────────────────────────────────────────────
        Scancode::NumLockClear => 0x90, // VK_NUMLOCK
        Scancode::ScrollLock   => 0x91, // VK_SCROLL

        // ── Sided modifiers ───────────────────────────────────────────────────
        Scancode::LShift => 0xA0, // VK_LSHIFT
        Scancode::RShift => 0xA1, // VK_RSHIFT
        Scancode::LCtrl  => 0xA2, // VK_LCONTROL
        Scancode::RCtrl  => 0xA3, // VK_RCONTROL
        Scancode::LAlt   => 0xA4, // VK_LMENU
        Scancode::RAlt   => 0xA5, // VK_RMENU

        // ── OEM punctuation (US layout positions) ─────────────────────────────
        Scancode::Semicolon        => 0xBA, // VK_OEM_1      ;:
        Scancode::Equals           => 0xBB, // VK_OEM_PLUS   =+
        Scancode::Comma            => 0xBC, // VK_OEM_COMMA  ,<
        Scancode::Minus            => 0xBD, // VK_OEM_MINUS  -_
        Scancode::Period           => 0xBE, // VK_OEM_PERIOD .>
        Scancode::Slash            => 0xBF, // VK_OEM_2      /?
        Scancode::Grave            => 0xC0, // VK_OEM_3      `~
        Scancode::LeftBracket      => 0xDB, // VK_OEM_4      [{
        Scancode::Backslash        => 0xDC, // VK_OEM_5      \|
        Scancode::RightBracket     => 0xDD, // VK_OEM_6      ]}
        Scancode::Apostrophe       => 0xDE, // VK_OEM_7      '"
        Scancode::NonUsBackslash   => 0xE2, // VK_OEM_102    ISO extra key

        _ => return None,
    })
}

pub fn key_event(scancode: Scancode, pressed: bool) -> Option<InputEvent> {
    Some(InputEvent {
        kind: if pressed { InputKind::KeyDown } else { InputKind::KeyUp },
        _pad: [0; 3],
        code: vk_code(scancode)?,
        x: 0,
        y: 0,
        flags: 0,
    })
}
