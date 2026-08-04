//! The connecting screen's backdrop: which game's wide art it wants, the one decoded
//! image kept for it, and the clocks that fade that image in, pan it, and fade it out
//! again once the stream is ready.
//!
//! One struct rather than seven `App` fields, because none of them mean anything on their
//! own — every launch moves all of them together.
use std::time::{Duration, Instant};

use crate::services::art::HeroImage;
use crate::ui;

#[derive(Default)]
pub(crate) struct Hero {
    /// The decoded image and the game id it belongs to. Several MB, and only one can ever
    /// be on screen, so a newer one simply replaces it.
    image: Option<(String, HeroImage)>,
    /// Last id asked for (the focused card), so an image that arrives after focus has
    /// moved on can be recognised as no longer useful.
    wanted: Option<String>,
    /// Id whose hero belongs on the connecting screen. `None` for Desktop.
    target: Option<String>,
    /// Whether the launching game has wide art at all: the menu loop waits a moment for
    /// art still in flight, but must not delay a game that will never have any.
    expected: bool,
    /// Id currently uploaded as `TileId::Hero`. Uploaded only once a launch starts —
    /// browsing must not put a multi-MB texture on the GPU.
    uploaded: Option<String>,
    /// When the uploaded image started fading in. Its own clock, not the launch fade's: a
    /// hero can land mid-handshake, and then it fades in from there.
    since: Option<Instant>,
    /// When it started fading back out, i.e. when the stream became ready.
    fade_out: Option<Instant>,
}

impl Hero {
    /// Notes whose hero is worth keeping — the focused card, whose art is prefetched.
    pub(crate) fn want(&mut self, game_id: &str) {
        self.wanted = Some(game_id.to_string());
    }

    /// Arms the loading screen for a launch of `target` (`None` = Desktop), `expected`
    /// saying whether that game has wide art to wait for.
    pub(crate) fn arm(&mut self, target: Option<String>, expected: bool) {
        self.target = target;
        self.expected = expected;
    }

    /// Takes a freshly decoded image, and reports whether it was kept. A `false` means the
    /// caller should let the loader forget it, so focusing that card again re-requests it
    /// (from the disk cache by then) rather than never asking a second time.
    pub(crate) fn accept(&mut self, game_id: String, image: HeroImage) -> bool {
        let useful =
            self.wanted.as_deref() == Some(game_id.as_str()) || self.target.as_deref() == Some(game_id.as_str());
        if useful {
            self.image = Some((game_id, image));
        }
        useful
    }

    /// The id whose texture the launch needs but doesn't have yet, if any.
    pub(crate) fn pending_upload(&self) -> Option<String> {
        let target = self.target.as_deref()?;
        if self.uploaded.as_deref() == Some(target) {
            return None;
        }
        // Still fetching — the fade to black covers this, and the hero joins it whenever
        // it lands (possibly not at all, on a slow host).
        self.image
            .as_ref()
            .filter(|(loaded, _)| loaded == target)
            .map(|(loaded, _)| loaded.clone())
    }

    /// Records that `id`'s texture is now uploaded and starts its fade-in, returning any
    /// texture the caller should drop in exchange.
    pub(crate) fn mark_uploaded(&mut self, id: String) -> Option<String> {
        let stale = self.uploaded.replace(id);
        self.since = Some(Instant::now());
        stale
    }

    /// `id`'s decoded pixels, for the upload itself.
    pub(crate) fn image_for(&self, id: &str) -> Option<&HeroImage> {
        self.image.as_ref().filter(|(loaded, _)| loaded == id).map(|(_, im)| im)
    }

    /// The uploaded tile's id and pixels, once there is one to draw.
    pub(crate) fn visible(&self) -> Option<(&String, &HeroImage)> {
        let id = self.uploaded.as_ref()?;
        self.since?;
        self.image
            .as_ref()
            .filter(|(loaded, _)| loaded == id)
            .map(|(id, im)| (id, im))
    }

    /// How far into the pan the backdrop is.
    pub(crate) fn panned_for(&self) -> Duration {
        self.since.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// This frame's opacity factor, 0..=1: the fade-in, less the fade-out once that starts.
    pub(crate) fn opacity(&self) -> f32 {
        let out = self.fade_out.map_or(0.0, |t| ui::anim_frac(Some(t), ui::HERO_FADE_OUT));
        ui::anim_frac(self.since, ui::HERO_FADE) * (1.0 - out)
    }

    /// Whether an uploaded hero is on screen as the connecting backdrop.
    pub(crate) fn showing(&self) -> bool {
        self.since.is_some() && self.uploaded.is_some()
    }

    /// Whether the loading screen is finished, so the streaming loop can take the screen.
    /// Also what starts the fade-out, once everything else it waits on is satisfied.
    ///
    /// `presenting` is the decoder reporting live frames. The backdrop waits for it, so
    /// the fade-out is the last thing that happens before the plane is uncovered rather
    /// than leaving black in between — with no hero this is not consulted at all, and the
    /// plain fade to black hands over exactly as it always did.
    pub(crate) fn handover_ready(
        &mut self,
        launch_elapsed: Duration,
        connect_done: Option<Instant>,
        presenting: bool,
    ) -> bool {
        if launch_elapsed < ui::LAUNCH_FADE {
            return false;
        }
        // Capped whatever else is going on, so a connect that never returns can't strand
        // the app on a panning image.
        if launch_elapsed >= ui::HERO_LOADING_MAX {
            return true;
        }
        if !self.showing() {
            // Nothing on screen: hand over at the end of the fade, as it always did. A game
            // that *has* wide art gets a grace period first — on a cold cache the hero can
            // still be a fetch away, and would otherwise land just after the hand-off.
            return !self.expected || launch_elapsed >= ui::LAUNCH_FADE + ui::HERO_WAIT;
        }
        // Held until the stream is genuinely up: a beat past the handshake (the stream's
        // own first frames are black anyway) and until the decoder presents, so the
        // fade-out runs straight into live video. Also for its own minimum, so a late
        // arrival isn't cut mid-fade-in.
        let Some(done) = connect_done else { return false };
        let stream_up = presenting || done.elapsed() >= ui::STREAM_REVEAL_WAIT;
        stream_up && done.elapsed() >= ui::HERO_LINGER && self.shown_for(ui::HERO_MIN_SHOW) && self.faded_out()
    }

    /// Whether the backdrop has been up (fade-in included) for at least `least`.
    fn shown_for(&self, least: Duration) -> bool {
        self.since.is_some_and(|t| t.elapsed() >= least)
    }

    /// Starts the fade-out (idempotent) and reports whether it has finished.
    fn faded_out(&mut self) -> bool {
        let since = *self.fade_out.get_or_insert_with(Instant::now);
        since.elapsed() >= ui::HERO_FADE_OUT
    }
}
