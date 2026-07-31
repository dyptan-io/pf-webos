//! SDL2 audio-queue playback of punktfunk's Opus audio packets. Decode only (Opus →
//! PCM) happens here — NDL is video-only (see ndl.rs docs), so this is a completely
//! separate path from the video decode/punch-through plane.
use anyhow::Result;
use punktfunk_core::audio::{layout_for, AudioGapTracker};

/// 48 kHz, 5 ms frames — punktfunk's fixed audio framing (see punktfunk-core's
/// audio.rs doc comments and its `multistream_layout_roundtrips_with_channel_identity`
/// test, the canonical reference for both ends of this wire format).
const SAMPLE_RATE: u32 = 48_000;
const SAMPLES_PER_FRAME: usize = 240;
/// Max channels punktfunk ever negotiates (7.1) — sizes the scratch decode buffer.
const MAX_CHANNELS: usize = 8;

/// Device buffer: 512 frames keeps slack vs pump cadence, cuts latency by ~75ms.
/// Obtained spec logged at session start for on-device verification.
const DEVICE_BUFFER_FRAMES: u16 = 512;

/// Soft ceiling on SDL-queued audio: above this, drop packets to let queue drain.
/// WHY: full clear at `MAX_QUEUED_LAG_MS` was audible (100ms silence). Drops are ~5ms.
pub const SOFT_QUEUED_LAG_MS: u32 = 60;

/// Hard bound: backstop against burst between pump ticks. Clear costs one audible blip.
pub const MAX_QUEUED_LAG_MS: u32 = 100;

/// What [`AudioPlayer::play`] did with a packet — reported so the caller can log the
/// cases that are audible, and tell an over-full queue apart from a starved one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioEvent {
    Queued,
    /// The device buffer had run dry before this packet arrived: the main loop didn't
    /// feed it in time (a stall), and the gap was audible. Distinct from every
    /// queue-too-full case below — the remedy is the opposite direction.
    Underrun,
    /// Dropped above [`SOFT_QUEUED_LAG_MS`] to let the queue drain.
    Dropped,
    /// Cleared at [`MAX_QUEUED_LAG_MS`] — the loud one.
    Resnapped,
}

pub struct AudioPlayer {
    queue: sdl2::audio::AudioQueue<f32>,
    decoder: opus::MSDecoder,
    channels: usize,
    /// Detects packets lost on the wire so they can be concealed rather than skipped —
    /// see [`AudioPlayer::play`].
    gaps: AudioGapTracker,
}

impl AudioPlayer {
    /// `channels` is host-resolved (client MUST build decoder from this, not own request).
    pub fn new(sdl_audio: &sdl2::AudioSubsystem, channels: u8) -> Result<Self> {
        let layout = layout_for(channels, false);
        let decoder = opus::MSDecoder::new(SAMPLE_RATE, layout.streams, layout.coupled, layout.mapping)
            .map_err(|e| anyhow::anyhow!("opus MSDecoder::new: {e}"))?;
        let spec = sdl2::audio::AudioSpecDesired {
            freq: Some(SAMPLE_RATE as i32),
            channels: Some(layout.channels),
            samples: Some(DEVICE_BUFFER_FRAMES),
        };
        let queue = sdl_audio
            .open_queue::<f32, _>(None, &spec)
            .map_err(|e| anyhow::anyhow!("SDL open_queue: {e}"))?;
        queue.resume();
        Ok(Self {
            queue,
            decoder,
            channels: layout.channels as usize,
            gaps: AudioGapTracker::new(),
        })
    }

    /// The device's actually-negotiated spec — may differ from what was requested if
    /// the device doesn't support it exactly.
    pub fn spec(&self) -> &sdl2::audio::AudioSpec {
        self.queue.spec()
    }

    /// Decodes Opus packet (with PLC for losses) and queues PCM.
    /// Returns peak sample (diagnostic for silent input vs speaker failure) + `AudioEvent`.
    pub fn play(&mut self, seq: u32, opus_payload: &[u8]) -> Result<(f32, AudioEvent)> {
        let bytes_per_ms = SAMPLE_RATE / 1000 * self.channels as u32 * std::mem::size_of::<f32>() as u32;
        let queued = self.queue.size();

        // WHY: empty queue detects stall on feeding thread (opposite remedy from over-full).
        let underrun = queued == 0;

        if queued > bytes_per_ms * MAX_QUEUED_LAG_MS {
            self.queue.clear();
            self.queue_packet(opus_payload)?;
            return Ok((0.0, AudioEvent::Resnapped));
        }
        if queued > bytes_per_ms * SOFT_QUEUED_LAG_MS {
            // WHY: decode anyway (stateful codec); skip would corrupt state and follow-up.
            let _ = self.gaps.missing_before(seq);
            let mut pcm = [0f32; SAMPLES_PER_FRAME * MAX_CHANNELS];
            let _ = self.decoder.decode_float(opus_payload, &mut pcm, false);
            return Ok((0.0, AudioEvent::Dropped));
        }

        // Conceal whatever went missing immediately before this packet.
        for _ in 0..self.gaps.missing_before(seq) {
            let mut pcm = [0f32; SAMPLES_PER_FRAME * MAX_CHANNELS];
            let n = self
                .decoder
                .decode_float(&[], &mut pcm, false)
                .map_err(|e| anyhow::anyhow!("opus PLC decode: {e}"))?;
            self.queue
                .queue_audio(&pcm[..n * self.channels])
                .map_err(|e| anyhow::anyhow!("SDL queue_audio (PLC): {e}"))?;
        }

        let peak = self.queue_packet(opus_payload)?;
        Ok((
            peak,
            if underrun {
                AudioEvent::Underrun
            } else {
                AudioEvent::Queued
            },
        ))
    }

    /// Decodes one real packet into the device queue, returning its peak sample.
    fn queue_packet(&mut self, opus_payload: &[u8]) -> Result<f32> {
        let mut pcm = [0f32; SAMPLES_PER_FRAME * MAX_CHANNELS];
        let samples_per_channel = self
            .decoder
            .decode_float(opus_payload, &mut pcm, false)
            .map_err(|e| anyhow::anyhow!("opus decode_float: {e}"))?;
        let decoded = &pcm[..samples_per_channel * self.channels];
        let peak = decoded.iter().fold(0f32, |m, &s| m.max(s.abs()));
        self.queue
            .queue_audio(decoded)
            .map_err(|e| anyhow::anyhow!("SDL queue_audio: {e}"))?;
        Ok(peak)
    }
}
