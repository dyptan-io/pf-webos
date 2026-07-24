//! On-demand cover-art loading, with a disk cache.
//!
//! Art is fetched over the same mTLS-pinned management API as the library itself, decoded
//! with the pure-Rust `image` crate (no on-device libjpeg/libpng to find), and handed to
//! the UI as an owned `tiny_skia::Pixmap` — unlike an SDL2 `Texture` (which isn't `Send`,
//! borrowing a `TextureCreator` tied to the main thread's GL context), a `Pixmap` crosses
//! a channel as the actual drawable object.
//!
//! **This used to fetch and decode the entire library up front, and keep all of it.** On a
//! 365-title library that is ~365 sequential mTLS round-trips on every launch and host
//! switch, and — at `MAX_ART_DIMENSION` — on the order of 200 MB of decoded pixmaps held
//! for the whole session, in a 32-bit process on a TV with under 2 GB usable. The grid can
//! only ever show a couple of dozen cards at once.
//!
//! So: the UI **requests** the covers it is about to draw ([`ArtLoader::request`]) and
//! drops the ones it has scrolled far away from, and a disk cache makes coming back cheap.
//! The cache stores the *encoded* bytes exactly as fetched (tens of KB) rather than decoded
//! pixels (hundreds of KB), so it stays small and a cache hit still costs only a decode.
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use tiny_skia::{IntSize, Pixmap};

use crate::library::GameEntry;
use crate::ui::premultiply_rgba;

/// One decoded cover, ready to composite straight into the UI's frame `Painter`.
pub struct ArtLoaded {
    pub game_id: String,
    pub pixmap: Pixmap,
}

/// A cover the UI wants soon.
struct ArtRequest {
    game_id: String,
    /// Host-relative art path (`/api/v1/library/art/...`).
    path: String,
}

/// Cap on the longer side of a decoded cover. Source art (Steam CDN capsules etc.) commonly
/// runs past 1000px, but a card is ~260px wide at 1080p even in the widest layout (see
/// `ui::grid_card_size`), so anything above this is memory and decode time spent on pixels
/// the panel cannot show.
const MAX_ART_DIMENSION: u32 = 480;

/// Where cached encoded art lives, under the app's own writable directory.
fn cache_dir() -> PathBuf {
    let home = std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from);
    home.join("art-cache")
}

/// A filesystem-safe name for a store-qualified id (`steam:570`, `custom:12`).
fn cache_name(game_id: &str) -> String {
    game_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Background fetcher/decoder. Requests go in, decoded covers come out; both ends are
/// non-blocking for the UI thread.
pub struct ArtLoader {
    tx: Sender<ArtRequest>,
    rx: Receiver<ArtLoaded>,
    /// Ids already handed to the worker, so scrolling over the same card repeatedly
    /// doesn't queue it repeatedly.
    requested: HashSet<String>,
}

impl ArtLoader {
    pub fn spawn(
        host: String,
        mgmt_port: u16,
        identity: (String, String),
        fingerprint: Option<[u8; 32]>,
    ) -> Self {
        let (tx_req, rx_req) = std::sync::mpsc::channel::<ArtRequest>();
        let (tx_done, rx_done) = std::sync::mpsc::channel::<ArtLoaded>();
        std::thread::Builder::new()
            .name("punktfunk-webos-art".into())
            .spawn(move || worker(&host, mgmt_port, &identity, fingerprint, &rx_req, &tx_done))
            .expect("spawn art-loader thread");
        Self {
            tx: tx_req,
            rx: rx_done,
            requested: HashSet::new(),
        }
    }

    /// Asks for `game`'s cover if it hasn't been asked for already. Cheap enough to call
    /// for every card in the prefetch window every frame.
    pub fn request(&mut self, game: &GameEntry) {
        if self.requested.contains(&game.id) {
            return;
        }
        // Remember the id either way: a game with no art at all must not be re-queued
        // every frame forever.
        self.requested.insert(game.id.clone());
        let Some(path) = game.art.portrait.as_deref().or(game.art.header.as_deref()) else {
            return;
        };
        // A closed channel means the worker is gone; the card keeps its placeholder.
        let _ = self.tx.send(ArtRequest {
            game_id: game.id.clone(),
            path: path.to_string(),
        });
    }

    /// Forgets that `game_id` was requested, so a later scroll back re-requests it. Served
    /// from the disk cache, so this costs a decode rather than a round-trip.
    pub fn forget(&mut self, game_id: &str) {
        self.requested.remove(game_id);
    }

    /// Drains everything decoded since the last call.
    pub fn drain(&self) -> Vec<ArtLoaded> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(loaded) => out.push(loaded),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }
}

fn worker(
    host: &str,
    mgmt_port: u16,
    identity: &(String, String),
    fingerprint: Option<[u8; 32]>,
    rx: &Receiver<ArtRequest>,
    tx: &Sender<ArtLoaded>,
) {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    // One mTLS agent reused for every fetch — a fresh `ureq::Agent` per cover means a
    // fresh TCP+TLS handshake including client-cert auth, real avoidable cost that scales
    // with library size (see `library::agent`). Built lazily, so a fully cached library
    // never opens a connection at all.
    let mut agent = None;

    while let Ok(req) = rx.recv() {
        let cached = dir.join(cache_name(&req.game_id));
        let bytes = match std::fs::read(&cached) {
            Ok(b) if !b.is_empty() => b,
            _ => {
                if agent.is_none() {
                    let Ok(a) = crate::library::agent(identity, fingerprint) else {
                        continue;
                    };
                    agent = Some(a);
                }
                let Some(a) = agent.as_ref() else { continue };
                let Ok(fetched) = crate::library::fetch_art(a, host, mgmt_port, &req.path) else {
                    continue;
                };
                // Write-then-rename, never truncate-in-place: a kill mid-write would
                // otherwise leave a truncated file that gets served from cache forever
                // (same discipline, and the same reason, as `store::write_atomic`).
                let tmp = cached.with_extension("tmp");
                if std::fs::write(&tmp, &fetched).is_ok() {
                    let _ = std::fs::rename(&tmp, &cached);
                }
                fetched
            }
        };

        let Ok(decoded) = image::load_from_memory(&bytes) else {
            // Drop a cache entry that won't decode — otherwise it poisons this card for
            // the life of the install.
            let _ = std::fs::remove_file(&cached);
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
        // A receiver drop (host switched) ends the thread — nothing left to deliver to.
        if tx
            .send(ArtLoaded {
                game_id: req.game_id,
                pixmap,
            })
            .is_err()
        {
            return;
        }
    }
}
