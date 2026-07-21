//! SDL2 audio-queue playback of punktfunk's Opus audio packets. Decode only (Opus →
//! PCM) happens here — NDL is video-only (see ndl.rs docs), so this is a completely
//! separate path from the video decode/punch-through plane.
use anyhow::Result;
use punktfunk_core::audio::layout_for;

/// 48 kHz, 5 ms frames — punktfunk's fixed audio framing (see punktfunk-core's
/// audio.rs doc comments and its `multistream_layout_roundtrips_with_channel_identity`
/// test, the canonical reference for both ends of this wire format).
const SAMPLE_RATE: u32 = 48_000;
const SAMPLES_PER_FRAME: usize = 240;
/// Max channels punktfunk ever negotiates (7.1) — sizes the scratch decode buffer.
const MAX_CHANNELS: usize = 8;

/// Requested SDL device buffer, in sample frames (~10.7ms at 48kHz). Left at
/// `samples: None`, SDL2 picks "~46ms rounded up to a power of two"
/// (`prepare_audiospec` in `SDL_audio.c`) — 4096 frames at 48kHz, ~85ms of built-in
/// output latency before a single network/queue delay is even counted. 512 keeps a
/// couple of 5ms audio frames of slack against main.rs's pump cadence while cutting
/// that fixed latency by ~75ms. The obtained spec is logged at session start
/// (main.rs), so what the driver actually granted is verifiable on-device.
const DEVICE_BUFFER_FRAMES: u16 = 512;

/// Hard bound on SDL-queued (pre-device) audio. With main.rs's few-ms pump cadence
/// the queue normally holds well under a couple of frames; the only ways past this
/// bound are a post-network-stall burst (punktfunk-core delivering its backlog at
/// once) or slow host/TV sample-clock drift accumulating — both of which would
/// otherwise become permanent added audio latency, since a realtime stream never
/// drains a standing queue on its own. Clearing and resnapping to the freshest
/// packet costs one audible blip but restores sync.
pub const MAX_QUEUED_LAG_MS: u32 = 100;

pub struct AudioPlayer {
    queue: sdl2::audio::AudioQueue<f32>,
    decoder: opus::MSDecoder,
    channels: usize,
}

impl AudioPlayer {
    /// `channels` is the host-resolved `NativeClient::audio_channels` (2/6/8) — the
    /// client MUST build its decoder from this, never its own request (see
    /// `punktfunk_core::client::NativeClient::audio_channels` docs).
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
        })
    }

    /// The device's actually-negotiated spec — may differ from what was requested if
    /// the device doesn't support it exactly.
    pub fn spec(&self) -> &sdl2::audio::AudioSpec {
        self.queue.spec()
    }

    /// Decodes one Opus packet and queues the resulting PCM for playback. Returns the
    /// decoded frame's peak absolute sample value — diagnostic for telling "silent
    /// input" apart from "output path not reaching the speaker" (see session.rs) —
    /// and whether the queue was cleared first because it had grown past
    /// [`MAX_QUEUED_LAG_MS`] (the caller logs that; see the const's docs).
    pub fn play(&mut self, opus_payload: &[u8]) -> Result<(f32, bool)> {
        let mut pcm = [0f32; SAMPLES_PER_FRAME * MAX_CHANNELS];
        let samples_per_channel = self
            .decoder
            .decode_float(opus_payload, &mut pcm, false)
            .map_err(|e| anyhow::anyhow!("opus decode_float: {e}"))?;
        let decoded = &pcm[..samples_per_channel * self.channels];
        let peak = decoded.iter().fold(0f32, |m, &s| m.max(s.abs()));
        let max_queued_bytes =
            SAMPLE_RATE / 1000 * MAX_QUEUED_LAG_MS * self.channels as u32 * std::mem::size_of::<f32>() as u32;
        let resnapped = self.queue.size() > max_queued_bytes;
        if resnapped {
            self.queue.clear();
        }
        self.queue
            .queue_audio(decoded)
            .map_err(|e| anyhow::anyhow!("SDL queue_audio: {e}"))?;
        Ok((peak, resnapped))
    }
}
