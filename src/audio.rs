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

pub struct AudioPlayer {
    queue: sdl2::audio::AudioQueue<f32>,
    decoder: opus::MSDecoder,
    channels: usize,
}

impl AudioPlayer {
    /// `channels` is the host-resolved `NativeClient::audio_channels` (2/6/8) — the
    /// client MUST build its decoder from this, never its own request (see
    /// `punktfunk_core::client::NativeClient::audio_channels` docs).
    pub fn new(sdl_audio: &sdl2::AudioSubsystem, channels: u8) -> Result<AudioPlayer> {
        let layout = layout_for(channels, false);
        let decoder =
            opus::MSDecoder::new(SAMPLE_RATE, layout.streams, layout.coupled, layout.mapping)
                .map_err(|e| anyhow::anyhow!("opus MSDecoder::new: {e}"))?;
        let spec = sdl2::audio::AudioSpecDesired {
            freq: Some(SAMPLE_RATE as i32),
            channels: Some(layout.channels),
            samples: None,
        };
        let queue = sdl_audio
            .open_queue::<f32, _>(None, &spec)
            .map_err(|e| anyhow::anyhow!("SDL open_queue: {e}"))?;
        queue.resume();
        Ok(AudioPlayer {
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
    /// input" apart from "output path not reaching the speaker" (see session.rs).
    pub fn play(&mut self, opus_payload: &[u8]) -> Result<f32> {
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
