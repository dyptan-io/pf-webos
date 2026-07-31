//! Adaptive triggers, lightbar and player LEDs on a real `DualSense`, over webOS's
//! Bluetooth HID plane.
//!
//! **Why this route exists.** SDL's own `DualSense` support (`SDL_GameControllerSendEffect`,
//! present in the bundled fork) drives the pad through `/dev/hidraw*`, and a webOS app's jail
//! exposes no hidraw node at all — not even with the pad connected, and with no `hidraw`
//! class in `/sys` either (verified on webOS 10.3, non-rooted; see `docs/NOTES.md`). So the
//! effect bytes cannot go through SDL. What *is* reachable is the TV's own Bluetooth stack:
//! `com.webos.service.bluetooth2/hid/internal/sendData` writes an arbitrary HID output report
//! to a connected HID device, and it sits in the `public` API group a dev-mode install
//! already holds — no root required.
//!
//! Two hard-won details, both verified on-device:
//!   * the payload carries `reportData` as an int array **with no `reportId` field** — adding
//!     one makes the service reject the call with a generic schema error that names nothing,
//!     and `setReport` (the method that *does* take a report id) always answers "operation can
//!     not be performed at this time";
//!   * the report needs the same CRC32 the kernel's `hid-playstation` appends, over a `0xA2`
//!     seed byte. Without it the pad silently ignores a call the service reports as success.
//!
//! Rumble deliberately does **not** go through here: it reaches the pad as ordinary evdev
//! force feedback via SDL (`GameController::set_rumble`), which works for every controller
//! type rather than only this one. Reports built here never set the vibration valid-flag, so
//! they cannot fight the kernel's force-feedback state — see [`build_report`].
use std::fmt::Write as _;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

use punktfunk_core::quic::HidOutput;

/// `hid/internal/sendData` — the only one of the three HID methods that works. `getReport`
/// hangs on a pad that doesn't answer; `setReport` refuses with error 4 whatever the payload.
const SEND_DATA_URI: &str = "luna://com.webos.service.bluetooth2/hid/internal/sendData";

/// Bluetooth `DualSense` output report, per Linux `hid-playstation`'s
/// `dualsense_output_report_bt`: `0x31`, seq/tag, tag, the 47-byte common block, 24 reserved
/// bytes, then the CRC32. Exactly this length was verified accepted end-to-end.
const REPORT_LEN: usize = 78;
/// Where the 47-byte common block starts (after report id, `seq_tag`, tag).
const COMMON: usize = 3;

/// `valid_flag0` bit: the right (R2) trigger effect block is meaningful in this report.
const FLAG0_RIGHT_TRIGGER: u8 = 0x04;
/// `valid_flag0` bit: the left (L2) trigger effect block is meaningful in this report.
const FLAG0_LEFT_TRIGGER: u8 = 0x08;
/// `valid_flag1` bit: take over the lightbar colour.
const FLAG1_LIGHTBAR: u8 = 0x04;
/// `valid_flag1` bit: drive the five player-indicator LEDs.
const FLAG1_PLAYER_LEDS: u8 = 0x10;

/// Common-block offset of the right trigger's effect block.
const OFF_RIGHT_TRIGGER: usize = 10;
/// Common-block offset of the left trigger's effect block.
const OFF_LEFT_TRIGGER: usize = 21;
/// Common-block offset of the player-indicator LED bits.
const OFF_PLAYER_LEDS: usize = 43;
/// Common-block offset of lightbar red; green and blue follow it.
const OFF_LIGHTBAR_RED: usize = 44;

/// A `DualSense` trigger parameter block: effect mode byte plus up to ten parameters. Sized to
/// match `punktfunk_core::abi::PUNKTFUNK_HID_EFFECT_MAX`, which is what the host forwards
/// from the game — so a block copies straight in with no reinterpretation.
const EFFECT_LEN: usize = 11;
type Effect = [u8; EFFECT_LEN];

/// Everything this client drives on the pad, as absolute state rather than deltas.
///
/// Absolute on purpose: one report carries all of it, so re-sending the whole struct makes
/// every update idempotent and self-healing. The host's feedback plane is lossy by design
/// (newest-drops on overflow, no retransmit), and a dropped *delta* would leave a trigger
/// stuck resisting until the game happened to change it again.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct State {
    /// `None` until a game sets a colour — the system keeps painting the lightbar until then.
    lightbar: Option<(u8, u8, u8)>,
    /// `None` until a game sets them, for the same reason as `lightbar`.
    player_leds: Option<u8>,
    /// Mode `0x00` (the default) means "no resistance", so the zero value is already correct.
    right_trigger: Effect,
    left_trigger: Effect,
    /// Whether to assert the trigger valid-flags at all. Travels *with* the state rather than
    /// being re-derived when sending: an explicit release is mode `0x00` on both triggers,
    /// which is indistinguishable from "never touched" by looking at the bytes — deriving it
    /// would drop the one report that lets go of a stiffened trigger.
    triggers_owned: bool,
}

/// CRC32 (IEEE 802.3, reflected) — the `crc32_le` variant `hid-playstation` signs Bluetooth
/// output reports with. Computed bitwise: 78 bytes per report at human-paced update rates
/// isn't worth a 1 KiB lookup table.
fn crc32_le(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            // 0xEDB88320 = reflected 0x04C11DB7.
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The pad's Bluetooth address, in the form `hid/internal/sendData`'s `address` wants.
///
/// Read from `/proc/bus/input/devices`' `U: Uniq=` line, which `hid-playstation` sets to the
/// pad's MAC. A `DualSense` publishes three input devices (pad, motion sensors, touchpad) all
/// sharing one `Uniq`, so the first match is the right one. `None` for a USB-connected pad
/// (no `Uniq`), which is correct — this path is Bluetooth-only.
pub fn find_address() -> Option<String> {
    let devices = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
    devices.split("\n\n").find_map(|block| {
        let is_dualsense = block
            .lines()
            .any(|l| l.starts_with("N: Name=") && l.to_ascii_lowercase().contains("dualsense"));
        if !is_dualsense {
            return None;
        }
        block
            .lines()
            .find_map(|l| l.strip_prefix("U: Uniq="))
            .map(|u| u.trim().to_ascii_lowercase())
            .filter(|u| !u.is_empty())
    })
}

/// Owns the pad's feedback state and the thread that ships it.
///
/// The thread exists because a send is a fork/exec of `luna-send-pub` (see [`crate::platform::webos::luna`]),
/// which must never land on the render/input loop. Queue depth is one with latest-wins
/// replacement — the same discipline as [`crate::services::store::SettingsWriter`] — because the state
/// is absolute: a superseded update carries no information the newer one lacks.
pub struct Feedback {
    state: State,
    /// `None` only after [`Drop`] has taken it to close the channel.
    tx: Option<SyncSender<State>>,
    /// `None` only after [`Drop`] has taken and joined it.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Feedback {
    /// Starts a feedback sender for the pad at Bluetooth `address`, or `None` when the
    /// platform can't support it (no `luna-send-pub`). Resolve `address` via [`find_address`].
    pub fn new(address: String) -> Option<Self> {
        if !crate::platform::webos::luna::available() {
            return None;
        }
        // Depth 1 + `try_send`: at most one pending state, and a full queue means the sender
        // is mid-call — that pending value is stale by definition, so the next update replaces
        // it rather than queueing behind it.
        let (tx, rx) = std::sync::mpsc::sync_channel::<State>(1);
        let thread = std::thread::Builder::new()
            .name("ds5-feedback".into())
            .spawn(move || sender_loop(&address, &rx))
            .ok()?;
        tracing::info!("DualSense feedback active (adaptive triggers, lightbar)");
        Some(Self {
            state: State::default(),
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// Folds one host feedback event into the pad state and queues a send.
    ///
    /// Variants this pad has no hardware for are dropped: `TrackpadHaptic` is a Steam
    /// Controller voice-coil buzz, and `HidRaw` is a passthrough report for a device the host
    /// mirrors as-is — replaying either on a `DualSense` would mean writing arbitrary bytes in
    /// the wrong protocol.
    pub fn apply(&mut self, event: &HidOutput) {
        match event {
            HidOutput::Led { r, g, b, .. } => self.state.lightbar = Some((*r, *g, *b)),
            HidOutput::PlayerLeds { bits, .. } => self.state.player_leds = Some(*bits),
            HidOutput::Trigger { which, effect, .. } => {
                let mut block = Effect::default();
                let n = effect.len().min(EFFECT_LEN);
                block[..n].copy_from_slice(&effect[..n]);
                // `which`: 0 = L2, 1 = R2 (`punktfunk_core::quic::HidOutput`).
                if *which == 0 {
                    self.state.left_trigger = block;
                } else {
                    self.state.right_trigger = block;
                }
                self.state.triggers_owned = true;
            }
            HidOutput::TrackpadHaptic { .. } | HidOutput::HidRaw { .. } => return,
        }
        // Dropping on a full queue *is* the coalescing: the waiting value is strictly older.
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(self.state);
        }
    }

    /// Releases everything this client took over — both triggers back to no resistance, the
    /// lightbar handed back to the system.
    ///
    /// Trigger resistance lives in the pad's firmware, not in the stream, so without this a
    /// game that left R2 stiff leaves it stiff on the TV's home screen and after the app
    /// exits, with nothing to connect that to punktfunk. Blocking send, unlike
    /// [`apply`](Self::apply): this one must not be the update that gets coalesced away, and
    /// [`Drop`] joins the sender right after so it is actually delivered.
    ///
    /// Called on the way out of a stream, so it delays return-to-menu by one send — tens of
    /// milliseconds normally, and at worst two [`crate::platform::webos::luna::CALL_TIMEOUT`] windows if the
    /// Bluetooth service has stopped answering.
    pub fn release(&mut self) {
        self.state = State {
            triggers_owned: true, // assert mode 0x00 explicitly rather than staying silent
            ..State::default()
        };
        if let Some(tx) = &self.tx {
            let _ = tx.send(self.state);
        }
    }
}

impl Drop for Feedback {
    fn drop(&mut self) {
        // Closing the channel ends `sender_loop`; joining lets a send in flight (and anything
        // `release` just queued) complete — the same reason `SettingsWriter` joins its writer.
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Monotonic report sequence, low 4 bits used — the pad expects a changing sequence number
/// per report.
fn seq_next() -> u8 {
    use std::sync::atomic::{AtomicU8, Ordering};
    static SEQ: AtomicU8 = AtomicU8::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed) & 0x0F
}

/// Builds one Bluetooth output report for `state`.
///
/// Note which valid-flags are never set: not `0x01` (compatible vibration), so the motor
/// bytes stay ignored and this report cannot cancel rumble that SDL's evdev force-feedback
/// path is driving independently.
fn build_report(seq: u8, state: &State) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[0] = 0x31; // Bluetooth output report id
    r[1] = (seq & 0x0F) << 4; // high nibble = sequence, low nibble = tag mask (0)
    r[2] = 0x10; // DS_OUTPUT_TAG
    if state.triggers_owned {
        r[COMMON] = FLAG0_RIGHT_TRIGGER | FLAG0_LEFT_TRIGGER;
        r[COMMON + OFF_RIGHT_TRIGGER..][..EFFECT_LEN].copy_from_slice(&state.right_trigger);
        r[COMMON + OFF_LEFT_TRIGGER..][..EFFECT_LEN].copy_from_slice(&state.left_trigger);
    }
    if let Some((red, green, blue)) = state.lightbar {
        r[COMMON + 1] |= FLAG1_LIGHTBAR;
        r[COMMON + OFF_LIGHTBAR_RED] = red;
        r[COMMON + OFF_LIGHTBAR_RED + 1] = green;
        r[COMMON + OFF_LIGHTBAR_RED + 2] = blue;
    }
    if let Some(bits) = state.player_leds {
        r[COMMON + 1] |= FLAG1_PLAYER_LEDS;
        r[COMMON + OFF_PLAYER_LEDS] = bits & 0x1F;
    }
    // CRC32 over a 0xA2 seed byte (the HIDP DATA/Output header the stack prepends) followed
    // by everything ahead of the CRC field itself.
    let mut signed = Vec::with_capacity(REPORT_LEN);
    signed.push(0xA2);
    signed.extend_from_slice(&r[..REPORT_LEN - 4]);
    r[REPORT_LEN - 4..].copy_from_slice(&crc32_le(&signed).to_le_bytes());
    r
}

/// `reportData` as a bare int array — **no `reportId` key**. The service validates the whole
/// object against a strict schema, so one extra property fails the call outright.
fn payload_for(address: &str, report: &[u8; REPORT_LEN]) -> String {
    let mut payload = String::with_capacity(REPORT_LEN * 4 + 64);
    let _ = write!(payload, "{{\"address\":\"{address}\",\"reportData\":[");
    for (i, b) in report.iter().enumerate() {
        let _ = if i == 0 {
            write!(payload, "{b}")
        } else {
            write!(payload, ",{b}")
        };
    }
    payload.push_str("]}");
    payload
}

fn send_report(address: &str, report: &[u8; REPORT_LEN]) -> anyhow::Result<()> {
    crate::platform::webos::luna::call(SEND_DATA_URI, &payload_for(address, report))
}

/// Minimum wall time between two sends.
///
/// A send is a fork/exec of `luna-send-pub` (see [`crate::platform::webos::luna`]), which copies the page
/// tables of a process holding SDL, the decoder and its frame buffers. **Unthrottled, this
/// blacked out the video plane on a real stream**: a Steam/Gamescope host animates the
/// `DualSense` lightbar continuously, which turned every animation step into a process spawn —
/// dozens per second on a 2-3 core TV. Frames kept decoding (that thread is priority-boosted)
/// while the compositor never got to present, so the panel stayed black with the frame counter
/// climbing and nothing dropped.
///
/// 250 ms is far finer than a trigger effect meaningfully changes (weapon swaps, state
/// transitions) and caps the cost at four spawns a second in the worst case.
const MIN_SEND_INTERVAL: Duration = Duration::from_millis(250);

/// Drains queued states, sending each. Ends when the channel closes (`Feedback` dropped).
///
/// Two guards keep the spawn rate down, both necessary: identical states are dropped outright
/// (a host re-asserting the same lightbar colour costs nothing), and the remainder are spaced
/// by [`MIN_SEND_INTERVAL`]. Sleeping *after* a send rather than dropping the update is what
/// makes the throttle lossless — the depth-1 channel keeps replacing the pending state while
/// this thread waits, so the newest one goes out next.
///
/// One log per run of failures, not per send: if the Bluetooth service stops accepting (pad
/// powered off mid-session) every later update would otherwise repeat the same line for as
/// long as the game keeps changing effects.
fn sender_loop(address: &str, rx: &Receiver<State>) {
    let mut failing = false;
    let mut last_sent: Option<State> = None;
    let mut sends: u32 = 0;
    while let Ok(state) = rx.recv() {
        if last_sent == Some(state) {
            continue;
        }
        match send_report(address, &build_report(seq_next(), &state)) {
            Ok(()) => {
                failing = false;
                last_sent = Some(state);
                sends += 1;
                // Rate visible in the log without one line per send — the symptom this
                // throttle exists for is invisible from the client's own counters.
                if sends % 50 == 0 {
                    tracing::debug!("DualSense feedback: {sends} reports sent");
                }
            }
            Err(e) => {
                if !failing {
                    tracing::warn!("DualSense feedback send failed (further errors quiet): {e}");
                    failing = true;
                }
            }
        }
        std::thread::sleep(MIN_SEND_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CRC must match `hid-playstation`'s `crc32_le`, or the pad ignores reports the
    /// Bluetooth service still reports as delivered — a failure mode with no visible signal,
    /// so it gets a test rather than trust.
    #[test]
    fn crc32_matches_known_vector() {
        // Standard IEEE CRC32 check value for "123456789".
        assert_eq!(crc32_le(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn report_shape_and_flags() {
        let state = State {
            lightbar: Some((1, 2, 3)),
            player_leds: Some(0xFF),
            right_trigger: [0x01, 0x00, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0],
            left_trigger: [0x02, 0x02, 0x07, 0xFF, 0, 0, 0, 0, 0, 0, 0],
            triggers_owned: true,
        };
        let r = build_report(3, &state);
        assert_eq!(r.len(), REPORT_LEN);
        assert_eq!(r[0], 0x31);
        assert_eq!(r[1], 0x30, "sequence rides the high nibble");
        assert_eq!(r[2], 0x10);
        assert_eq!(r[COMMON], FLAG0_RIGHT_TRIGGER | FLAG0_LEFT_TRIGGER);
        assert_eq!(r[COMMON + 1], FLAG1_LIGHTBAR | FLAG1_PLAYER_LEDS);
        assert_eq!(r[COMMON + OFF_RIGHT_TRIGGER], 0x01, "continuous-resistance mode");
        assert_eq!(r[COMMON + OFF_LEFT_TRIGGER], 0x02, "section-resistance mode");
        assert_eq!(r[COMMON + OFF_PLAYER_LEDS], 0x1F, "player LEDs masked to 5 bits");
        assert_eq!(
            (
                r[COMMON + OFF_LIGHTBAR_RED],
                r[COMMON + OFF_LIGHTBAR_RED + 1],
                r[COMMON + OFF_LIGHTBAR_RED + 2],
            ),
            (1, 2, 3)
        );
        // Vibration flag must stay clear so this cannot fight evdev force feedback.
        assert_eq!(r[COMMON] & 0x01, 0);
    }

    /// A game that never sets an effect must leave the pad's own trigger feel alone.
    #[test]
    fn triggers_untouched_until_a_game_sets_one() {
        let state = State {
            lightbar: Some((9, 9, 9)),
            ..State::default()
        };
        let r = build_report(0, &state);
        assert_eq!(r[COMMON] & (FLAG0_RIGHT_TRIGGER | FLAG0_LEFT_TRIGGER), 0);
    }

    /// Releasing is mode `0x00` on both triggers *with* the valid-flags set — the bytes look
    /// identical to "never touched", which is why `triggers_owned` is carried explicitly.
    #[test]
    fn release_state_still_asserts_trigger_flags() {
        let released = State {
            triggers_owned: true,
            ..State::default()
        };
        let r = build_report(0, &released);
        assert_eq!(r[COMMON], FLAG0_RIGHT_TRIGGER | FLAG0_LEFT_TRIGGER);
        assert_eq!(r[COMMON + OFF_RIGHT_TRIGGER], 0x00);
        assert_eq!(r[COMMON + OFF_LEFT_TRIGGER], 0x00);
    }

    /// The exact payload shape the service accepts. An extra `reportId` key — the intuitive
    /// thing to add, since the report *has* an id byte — fails the whole call with a schema
    /// error that names no field, so the absence is asserted rather than assumed.
    #[test]
    fn payload_shape_has_no_report_id_key() {
        let payload = payload_for("aa:bb", &build_report(0, &State::default()));
        assert!(!payload.contains("reportId"), "an extra key fails the service schema");
        // 49 = 0x31, the Bluetooth output report id, as the first array element.
        assert!(
            payload.starts_with("{\"address\":\"aa:bb\",\"reportData\":[49,"),
            "{payload}"
        );
        assert!(payload.ends_with("]}"));
    }
}
