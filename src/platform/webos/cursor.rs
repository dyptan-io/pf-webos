//! Hiding the local pointer during a stream takes both layers webOS can draw one from:
//!
//! * **SDL's cursor** — `MouseUtil::show_cursor`. Window-scoped, restored when the
//!   process dies. The layer that has always worked.
//! * **The compositor's pointer** — `wl_webos_input_manager.set_cursor_visibility` via
//!   the SDL-webOS `SDL_webOSCursorVisibility` extension. Some webOS versions put this
//!   back on pointer activity and SDL can't undo it: `SDL_ShowCursor` only reaches the
//!   backend when its cached `cursor_shown` *flips*, so a repeat hide is a silent
//!   no-op, and `is_cursor_showing()` reads that same cache — it reports "hidden"
//!   while the TV visibly draws an arrow.
//!
//! That second call is global state, so [`restore_on_exit`] covers deaths that skip
//! [`Cursor`]'s own teardown.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use sdl2::mouse::MouseUtil;
use sdl2::sys::SDL_bool;

extern "C" {
    /// SDL-webOS extension (`SDL_system.h`); `SDL_FALSE` when the compositor never
    /// advertised `wl_webos_input_manager`, which is how this self-gates on TVs
    /// that don't have it rather than by guessing at a version number.
    fn SDL_webOSCursorVisibility(visible: SDL_bool) -> SDL_bool;
}

/// Nothing announces the compositor taking its pointer back — the protocol's
/// `cursor_visibility` event isn't exposed by SDL — so re-asserting polls on pointer
/// activity instead, capped here at four Wayland requests a second.
const REASSERT_INTERVAL: Duration = Duration::from_millis(250);

/// Global because the compositor request is, and because the panic hook has no
/// [`Cursor`] to reach for. `OWNER_THREAD` owns the Wayland connection, so
/// [`restore_on_exit`] won't touch the cursor from any other thread.
static COMPOSITOR_HIDDEN: AtomicBool = AtomicBool::new(false);
static SUPPORT_LOGGED: AtomicBool = AtomicBool::new(false);
static OWNER_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// The local pointer, on every layer that can draw one. Drive from the SDL video thread.
pub struct Cursor {
    mouse: MouseUtil,
    last_assert: Instant,
}

impl Cursor {
    pub fn new(mouse: MouseUtil) -> Self {
        Self {
            mouse,
            last_assert: Instant::now(),
        }
    }

    /// Show or hide it on both layers.
    pub fn set_visible(&mut self, visible: bool) {
        let _ = OWNER_THREAD.set(std::thread::current().id());
        self.mouse.show_cursor(visible);
        set_compositor_visible(visible);
        COMPOSITOR_HIDDEN.store(!visible, Ordering::Relaxed);
        self.last_assert = Instant::now();
    }

    /// Call on pointer activity; re-asserts the hide when due. No-op while the pointer
    /// is meant to be visible, so callers needn't check the setting.
    pub fn on_pointer_activity(&mut self) {
        if !COMPOSITOR_HIDDEN.load(Ordering::Relaxed) {
            return;
        }
        if self.last_assert.elapsed() < REASSERT_INTERVAL {
            return;
        }
        self.last_assert = Instant::now();
        set_compositor_visible(false);
    }
}

fn set_compositor_visible(visible: bool) -> bool {
    // SAFETY: plain integer argument, no pointers; caller is the SDL video thread.
    let supported = unsafe { SDL_webOSCursorVisibility(bool_to_sdl(!visible)) } == SDL_bool::SDL_TRUE;
    // Once, not per call — a stray-cursor bug report needs to say whether the TV has
    // the interface at all.
    if !SUPPORT_LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "compositor cursor visibility control: {}",
            if supported { "available" } else { "unavailable" }
        );
    }
    supported
}

const fn bool_to_sdl(value: bool) -> SDL_bool {
    if value {
        SDL_bool::SDL_TRUE
    } else {
        SDL_bool::SDL_FALSE
    }
}

/// Put the compositor pointer back if a [`Cursor`] hid it — for exits that skip its
/// teardown (currently the panic hook); a graceful quit already calls
/// [`Cursor::set_visible`]`(true)`.
///
/// No-op off the hiding thread: a panicking video-pump thread has no business in the
/// main thread's Wayland connection, and the override drops on client disconnect.
pub fn restore_on_exit() {
    if !COMPOSITOR_HIDDEN.swap(false, Ordering::Relaxed) {
        return;
    }
    if OWNER_THREAD.get() != Some(&std::thread::current().id()) {
        tracing::warn!("cursor left hidden — panic is off the SDL thread, leaving it to client teardown");
        return;
    }
    set_compositor_visible(true);
}
