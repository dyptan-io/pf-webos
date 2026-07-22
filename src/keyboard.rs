//! Maps SDL2 keyboard events to punktfunk's wire `InputEvent`s. The webOS Magic
//! Remote's 5-way pad surfaces as plain SDL2 `Keycode::Up/Down/Left/Right` (same as
//! any desktop arrow key), forwarded to the host as real key presses during a stream
//! so directional navigation drives the host UI instead of only the local menu (see
//! `ui::menu_event_for_key`, used pre-stream for the same keycodes).
use punktfunk_core::input::{InputEvent, InputKind};
use sdl2::keyboard::Keycode;

/// Windows VK code `punktfunk-host`'s injector expects in a `KeyDown`/`KeyUp`'s `code`
/// (confirmed via `vk_to_evdev` in `punktfunk-host/pf-inject/src/inject/keymap.rs`).
/// `None` for any key not yet forwarded during streaming.
fn vk_code(keycode: Keycode) -> Option<u32> {
    match keycode {
        Keycode::Left => Some(0x25),
        Keycode::Up => Some(0x26),
        Keycode::Right => Some(0x27),
        Keycode::Down => Some(0x28),
        _ => None,
    }
}

pub fn key_event(keycode: Keycode, pressed: bool) -> Option<InputEvent> {
    Some(InputEvent {
        kind: if pressed { InputKind::KeyDown } else { InputKind::KeyUp },
        _pad: [0; 3],
        code: vk_code(keycode)?,
        x: 0,
        y: 0,
        flags: 0,
    })
}
