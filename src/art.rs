//! Background cover-art fetch + decode. `library::fetch_art` gets raw JPEG/PNG
//! bytes over the same mTLS-pinned management API `library::fetch_games` uses;
//! decoding (via the pure-Rust `image` crate — no on-device libjpeg/libpng needed)
//! happens on a dedicated thread so a slow/large library never blocks the UI loop.
//! Each decoded cover becomes a `tiny_skia::Pixmap` right here — unlike an SDL2
//! `Texture` (which isn't `Send`, since it borrows a `TextureCreator` tied to the
//! main thread's GL/window context), a `Pixmap` is a plain owned buffer, so it
//! can cross the channel to the UI thread as the actual drawable object, with no
//! separate GPU-texture-building/caching step over there.
use std::sync::mpsc::{Receiver, Sender};

use tiny_skia::{IntSize, Pixmap};

use crate::library::GameEntry;
use crate::ui::premultiply_rgba;

/// One decoded cover, ready to composite straight into the UI's frame `Painter`.
pub struct ArtLoaded {
    pub game_id: String,
    pub pixmap: Pixmap,
}

/// Spawns one background thread that fetches+decodes every game's art (preferring
/// `portrait`, falling back to `header` — the two orientations actually meant for a
/// grid; `hero`/`logo` are for other layouts this client doesn't have), sending each
/// as it's ready. Best-effort per title: a fetch/decode failure just means that
/// card keeps its placeholder — never fails the whole batch.
pub fn load_art_async(
    host: String,
    mgmt_port: u16,
    identity: (String, String),
    fingerprint: Option<[u8; 32]>,
    games: Vec<GameEntry>,
) -> Receiver<ArtLoaded> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("punktfunk-webos-art".into())
        .spawn(move || fetch_all(&host, mgmt_port, &identity, fingerprint, &games, &tx))
        .expect("spawn art-loader thread");
    rx
}

/// Cap on the longer side of a decoded cover before it becomes a `Pixmap`. Source
/// art (Steam CDN capsules etc.) commonly runs well past 1000px on a side, but the
/// grid never draws a card anywhere near that — `ui::CARD_MIN_W` is 220px and even
/// a 4K panel at the minimum 2-column layout tops out a few hundred px short of
/// this cap (see `ui::grid_card_size`). Decoding at full source resolution wastes
/// both host-thread decode time and (durably, for as long as the card is in
/// `App::art`) buffer memory for pixels the panel can never actually show.
const MAX_ART_DIMENSION: u32 = 480;

fn fetch_all(
    host: &str,
    mgmt_port: u16,
    identity: &(String, String),
    fingerprint: Option<[u8; 32]>,
    games: &[GameEntry],
    tx: &Sender<ArtLoaded>,
) {
    // One mTLS connection reused for every game's art in this batch — building a
    // fresh `ureq::Agent` per request (the old code's `fetch_art` did this
    // internally) means a fresh TCP+TLS handshake, including client-cert auth,
    // for every single cover: real, avoidable latency and CPU cost that scales
    // with library size. See `library::agent`'s docs.
    let Ok(agent) = crate::library::agent(identity, fingerprint) else {
        return;
    };
    for game in games {
        let Some(path) = game.art.portrait.as_deref().or(game.art.header.as_deref()) else {
            continue;
        };
        let Ok(bytes) = crate::library::fetch_art(&agent, host, mgmt_port, path) else {
            continue;
        };
        let Ok(decoded) = image::load_from_memory(&bytes) else {
            continue;
        };
        let longer_side = decoded.width().max(decoded.height());
        let decoded = if longer_side > MAX_ART_DIMENSION {
            decoded.resize(
                MAX_ART_DIMENSION,
                MAX_ART_DIMENSION,
                image::imageops::FilterType::Triangle,
            )
        } else {
            decoded
        };
        let rgba = decoded.to_rgba8();
        let (width, height) = rgba.dimensions();
        let Some(size) = IntSize::from_wh(width, height) else {
            continue;
        };
        let mut buf = rgba.into_raw();
        premultiply_rgba(&mut buf);
        let Some(pixmap) = Pixmap::from_vec(buf, size) else {
            continue;
        };
        // A receiver drop (host switched again before this batch finished) just
        // ends the thread early — nothing left to deliver to.
        if tx
            .send(ArtLoaded {
                game_id: game.id.clone(),
                pixmap,
            })
            .is_err()
        {
            return;
        }
    }
}
