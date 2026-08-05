//! One live stream, whichever protocol it speaks.
//!
//! An enum, not a trait: both implementations always exist in this build, so a trait would buy
//! dynamic dispatch while losing the compiler's totality check on a third protocol — the same
//! reason `Screen` is an enum. Verbs are named after the punktfunk ones the streaming loop already
//! called (`disconnect_quit`, `is_session_ended`, `shutdown`), so the loop reads unchanged.

use std::sync::Arc;

use punktfunk_core::input::InputEvent;

use crate::backend::gamestream::stream::{GsInput, GsStream};
use crate::platform::webos::audio::AudioPlayer;
use crate::session::{self, Connected, StreamStats};

pub(crate) enum StreamHandle {
    Punktfunk(Connected),
    GameStream(GsStream),
}

/// The input path, cloned for a thread that sends off the main loop (the HID-mouse reader).
#[derive(Clone)]
pub(crate) enum InputSender {
    Punktfunk(Arc<punktfunk_core::client::NativeClient>),
    GameStream(Arc<GsInput>),
}

impl InputSender {
    pub(crate) fn send(&self, ev: &InputEvent) {
        match self {
            Self::Punktfunk(client) => {
                let _ = session::send_input(client, ev);
            }
            Self::GameStream(input) => input.send(ev),
        }
    }
}

impl StreamHandle {
    pub(crate) fn input(&self) -> InputSender {
        match self {
            Self::Punktfunk(c) => InputSender::Punktfunk(c.client.clone()),
            Self::GameStream(s) => InputSender::GameStream(s.input()),
        }
    }

    pub(crate) fn send_input(&self, ev: &InputEvent) {
        match self {
            Self::Punktfunk(c) => {
                let _ = session::send_input(&c.client, ev);
            }
            Self::GameStream(s) => s.send_input(ev),
        }
    }

    pub(crate) fn stats(&self) -> &Arc<StreamStats> {
        match self {
            Self::Punktfunk(c) => &c.stats,
            Self::GameStream(s) => &s.stats,
        }
    }

    /// Whether HDR is being applied, for the Game-mode picture pick.
    pub(crate) fn hdr(&self) -> bool {
        match self {
            Self::Punktfunk(c) => c.hdr,
            Self::GameStream(s) => s.hdr,
        }
    }

    /// Channels to open the SDL audio device with, or `None` when the session needs no local
    /// device (punktfunk's NDL audio offload took the stream).
    pub(crate) fn audio_channels(&self) -> Option<u8> {
        match self {
            Self::Punktfunk(c) if c.audio_offloaded => None,
            Self::Punktfunk(c) => Some(c.client.audio_channels),
            Self::GameStream(_) => Some(GsStream::AUDIO_CHANNELS),
        }
    }

    /// Drains decoded audio into the device. Call once per tick.
    pub(crate) fn pump_audio_once(&self, audio: &mut AudioPlayer) {
        match self {
            Self::Punktfunk(c) => session::pump_audio_once(&c.client, audio),
            Self::GameStream(s) => s.pump_audio_once(audio),
        }
    }

    /// Drains the host→client pad feedback planes. `GameStream` carries its feedback on the
    /// control stream's listener, and the crate only delivers the lightbar there — so no pad
    /// rumble on that protocol, for the upstream reason `GsStream::pump_feedback_once` documents.
    pub(crate) fn pump_feedback_once(
        &self,
        controller: Option<&mut sdl2::controller::GameController>,
        feedback: Option<&mut crate::platform::webos::dualsense::Feedback>,
    ) {
        match self {
            Self::Punktfunk(c) => session::pump_feedback_once(&c.client, controller, feedback),
            Self::GameStream(s) => s.pump_feedback_once(feedback),
        }
    }

    pub(crate) fn is_session_ended(&self) -> bool {
        match self {
            Self::Punktfunk(c) => c.client.is_session_ended(),
            Self::GameStream(s) => s.is_session_ended(),
        }
    }

    /// The sentence for the menu toast when the session ended on its own. punktfunk has no signal
    /// that separates a graceful close from a drop, so it keeps one sentence for both;
    /// `GameStream`'s host sends a reason code (see `GsStream::end_message`).
    pub(crate) fn end_message(&self) -> String {
        match self {
            Self::Punktfunk(_) => "The host closed the connection".to_string(),
            Self::GameStream(s) => s.end_message(),
        }
    }

    pub(crate) fn disconnect_quit(&self) {
        match self {
            Self::Punktfunk(c) => c.client.disconnect_quit(),
            Self::GameStream(s) => s.disconnect_quit(),
        }
    }

    /// `false` = teardown timed out, so the caller must skip `ndl::quit()`.
    pub(crate) fn shutdown(self) -> bool {
        match self {
            Self::Punktfunk(c) => c.shutdown(),
            Self::GameStream(s) => s.shutdown(),
        }
    }

    /// The stats overlay's protocol-dependent figures. Grouped into one call so the overlay block
    /// has one match rather than six.
    pub(crate) fn overlay_info(&self) -> OverlayInfo {
        match self {
            Self::Punktfunk(c) => {
                let mode = c.client.mode();
                OverlayInfo {
                    width: mode.width,
                    height: mode.height,
                    refresh_hz: mode.refresh_hz,
                    codec: session::codec_name(c.client.codec).to_string(),
                    hdr: c.client.color.is_hdr(),
                    frames_dropped: Some(c.client.frames_dropped()),
                    fec_recovered: Some(c.client.fec_recovered_shards()),
                    // The encoder's CURRENT target, not the session-start negotiation: on
                    // Automatic the ABR re-targets mid-session. `0` = a host too old to report.
                    target_kbps: match c.client.current_bitrate_kbps() {
                        0 => c.client.resolved_bitrate_kbps,
                        live => live,
                    },
                }
            }
            Self::GameStream(s) => OverlayInfo {
                width: s.width,
                height: s.height,
                refresh_hz: s.refresh_hz,
                codec: format!("{:?}", s.codec),
                hdr: s.hdr,
                // The protocol has no client-visible drop or FEC-recovery counter; the overlay
                // prints "n/a" rather than a zero that would read as "no loss".
                frames_dropped: None,
                fec_recovered: None,
                // What `/launch` asked for, and it does not move: no host-side ABR.
                target_kbps: s.bitrate_kbps,
            },
        }
    }
}

/// See [`StreamHandle::overlay_info`].
pub(crate) struct OverlayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub codec: String,
    pub hdr: bool,
    pub frames_dropped: Option<u64>,
    pub fec_recovered: Option<u64>,
    pub target_kbps: u32,
}
