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

/// Requested SDL device buffer, in sample frames (~10.7ms at 48kHz). Left at
/// `samples: None`, SDL2 picks "~46ms rounded up to a power of two"
/// (`prepare_audiospec` in `SDL_audio.c`) — 4096 frames at 48kHz, ~85ms of built-in
/// output latency before a single network/queue delay is even counted. 512 keeps a
/// couple of 5ms audio frames of slack against main.rs's pump cadence while cutting
/// that fixed latency by ~75ms. The obtained spec is logged at session start
/// (main.rs), so what the driver actually granted is verifiable on-device.
const DEVICE_BUFFER_FRAMES: u16 = 512;

/// Soft ceiling on SDL-queued (pre-device) audio: above this, an incoming packet is
/// **dropped** instead of queued, and the queue is allowed to drain at realtime.
///
/// This is the gentle half of the latency bound. The causes of a growing queue — a
/// post-network-stall burst (punktfunk-core delivering its backlog at once) or slow
/// host/TV sample-clock drift — both need *something* discarded, because a realtime
/// stream never drains a standing queue on its own. Discarding one 5 ms packet is a far
/// smaller artifact than [`MAX_QUEUED_LAG_MS`]'s full clear, and at 5 ms per drop the
/// queue walks back down quickly.
///
/// This exists because the hard clear was audible: on-device logs showed five
/// `audio resnapped` events in a few minutes of streaming, each one a ~100 ms silence —
/// reported as intermittent crackling.
pub const SOFT_QUEUED_LAG_MS: u32 = 60;

/// Hard bound on SDL-queued audio — a backstop, not the normal mechanism.
///
/// With [`SOFT_QUEUED_LAG_MS`] dropping packets above 60 ms this should now be
/// unreachable in steady state; it stays as protection against a burst large enough to
/// arrive between two pump ticks. Clearing costs one audible blip but restores sync.
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
            gaps: AudioGapTracker::new(),
        })
    }

    /// The device's actually-negotiated spec — may differ from what was requested if
    /// the device doesn't support it exactly.
    pub fn spec(&self) -> &sdl2::audio::AudioSpec {
        self.queue.spec()
    }

    /// Decodes one Opus packet (concealing any lost before it) and queues the PCM.
    ///
    /// `seq` is the packet's wire sequence. punktfunk's audio datagrams carry **no FEC**,
    /// so a lost 5 ms packet used to play out as a hard gap — an audible click. Core ships
    /// `AudioGapTracker` for exactly this ("shared by every platform decoder"); this client
    /// was the one that never used it. Missing packets are now synthesized with libopus
    /// packet-loss concealment (`decode_float` with empty input maps to a NULL data
    /// pointer, which is libopus's PLC entry point) before the real packet is decoded,
    /// turning clicks into an inaudible interpolation.
    ///
    /// Returns the decoded frame's peak absolute sample — diagnostic for telling "silent
    /// input" from "output path not reaching the speaker" — and what happened to the
    /// packet ([`AudioEvent`]).
    pub fn play(&mut self, seq: u32, opus_payload: &[u8]) -> Result<(f32, AudioEvent)> {
        let bytes_per_ms = SAMPLE_RATE / 1000 * self.channels as u32 * std::mem::size_of::<f32>() as u32;
        let queued = self.queue.size();

        // An empty device queue means the audio device already ran dry — the gap has
        // happened. Reported so a stall on the feeding thread is distinguishable from the
        // over-full cases below; the two have opposite remedies.
        let underrun = queued == 0;

        if queued > bytes_per_ms * MAX_QUEUED_LAG_MS {
            self.queue.clear();
            self.queue_packet(opus_payload)?;
            return Ok((0.0, AudioEvent::Resnapped));
        }
        if queued > bytes_per_ms * SOFT_QUEUED_LAG_MS {
            // Dropped, but still fed to the decoder: Opus is a stateful codec, and
            // skipping a packet outright would leave the decoder's state behind the
            // stream and corrupt what follows. Decode, advance the gap tracker, discard
            // the samples.
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
        Ok((peak, if underrun { AudioEvent::Underrun } else { AudioEvent::Queued }))
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
