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

/// The grid card's fixed portrait aspect (see `ui::grid_card_size`). A card's art is
/// stretched to exactly fill its rect (`Painter::draw_pixmap_scaled`), so source art at a
/// different aspect ratio — a lot of it, in the wild (Steam capsules, custom box art) —
/// would otherwise look visibly squashed or stretched. Center-cropping to this aspect once
/// at decode time avoids that without ever distorting the image.
const TARGET_ART_ASPECT: f32 = 3.0 / 4.0;

/// Center-crops `img` to `aspect` (width/height), trimming whichever axis is oversized.
/// A no-op if `img` is already close enough to `aspect`.
fn crop_to_aspect(img: image::DynamicImage, aspect: f32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return img;
    }
    let current = w as f32 / h as f32;
    if (current - aspect).abs() < 0.01 {
        return img;
    }
    if current > aspect {
        let new_w = ((h as f32 * aspect).round() as u32).clamp(1, w);
        img.crop_imm((w - new_w) / 2, 0, new_w, h)
    } else {
        let new_h = ((w as f32 / aspect).round() as u32).clamp(1, h);
        img.crop_imm(0, (h - new_h) / 2, w, new_h)
    }
}

/// Bump when the raw-cache layout or `MAX_ART_DIMENSION` changes, to invalidate stale caches.
const RAW_CACHE_MAGIC: u32 = 0x50465232; // "PFR2" — bumped for center-cropped art

/// Root of all cached art, under the app's own writable directory.
fn cache_root() -> PathBuf {
    let home = std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from);
    home.join("art-cache")
}

/// A filesystem-safe name for a store-qualified id (`steam:570`, `custom:12`) or a
/// `host:port` key. Not collision-proof (e.g. `"a:1"` and `"a_1"` sanitize the same).
fn cache_name(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// One subdirectory per host (keyed by `(host, port)`, same as `store::KnownHost`'s dedup
/// key) — keeps game-id collisions across hosts from leaking art, and lets a forgotten
/// host's cache be dropped in one `remove_dir_all`.
fn cache_dir(host: &str, port: u16) -> PathBuf {
    cache_root().join(cache_name(&format!("{host}_{port}")))
}

/// Deletes a forgotten host's cached art. Best-effort: a missing or unremovable directory
/// is not an error the caller needs to react to.
pub fn clear_host_cache(host: &str, port: u16) {
    let _ = std::fs::remove_dir_all(cache_dir(host, port));
}

/// Decoded/resized/premultiplied pixels, keyed like the encoded cache. A hit here skips
/// `image::load_from_memory`, `resize`, and `premultiply_rgba` entirely.
fn raw_cache_path(dir: &std::path::Path, game_id: &str) -> PathBuf {
    dir.join(format!("{}.raw", cache_name(game_id)))
}

/// Layout: `[magic: u32 LE][width: u32 LE][height: u32 LE][premultiplied RGBA bytes]`.
fn write_raw_cache(path: &std::path::Path, pixmap: &Pixmap) {
    let mut buf = Vec::with_capacity(12 + pixmap.data().len());
    buf.extend_from_slice(&RAW_CACHE_MAGIC.to_le_bytes());
    buf.extend_from_slice(&pixmap.width().to_le_bytes());
    buf.extend_from_slice(&pixmap.height().to_le_bytes());
    buf.extend_from_slice(pixmap.data());
    // Write-then-rename so a kill mid-write can't leave a truncated file that parses as
    // garbage forever (same discipline as the encoded cache below).
    let tmp = path.with_extension("raw.tmp");
    if std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Reads back a raw cache entry written by [`write_raw_cache`], if present and well-formed.
fn read_raw_cache(path: &std::path::Path) -> Option<Pixmap> {
    let buf = std::fs::read(path).ok()?;
    let magic = u32::from_le_bytes(buf.get(0..4)?.try_into().ok()?);
    if magic != RAW_CACHE_MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(buf.get(4..8)?.try_into().ok()?);
    let height = u32::from_le_bytes(buf.get(8..12)?.try_into().ok()?);
    let size = IntSize::from_wh(width, height)?;
    let pixels = buf.get(12..)?.to_vec();
    Pixmap::from_vec(pixels, size)
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
    /// `port` is the host's identity port (matches `store::KnownHost::port`); `mgmt_port`
    /// is what's actually dialed to fetch art.
    pub fn spawn(
        host: String,
        port: u16,
        mgmt_port: u16,
        identity: (String, String),
        fingerprint: Option<[u8; 32]>,
    ) -> Self {
        let (tx_req, rx_req) = std::sync::mpsc::channel::<ArtRequest>();
        let (tx_done, rx_done) = std::sync::mpsc::channel::<ArtLoaded>();
        let dir = cache_dir(&host, port);
        std::thread::Builder::new()
            .name("punktfunk-webos-art".into())
            .spawn(move || worker(&host, mgmt_port, &identity, fingerprint, &dir, &rx_req, &tx_done))
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
    dir: &std::path::Path,
    rx: &Receiver<ArtRequest>,
    tx: &Sender<ArtLoaded>,
) {
    let _ = std::fs::create_dir_all(dir);
    // One mTLS agent reused for every fetch — a fresh `ureq::Agent` per cover means a
    // fresh TCP+TLS handshake including client-cert auth, real avoidable cost that scales
    // with library size (see `library::agent`). Built lazily, so a fully cached library
    // never opens a connection at all.
    let mut agent = None;

    // A closed channel means the host was switched away from; the caller stops draining.
    let deliver = |tx: &Sender<ArtLoaded>, game_id: String, pixmap: Pixmap| tx.send(ArtLoaded { game_id, pixmap });

    while let Ok(req) = rx.recv() {
        let raw_cached = raw_cache_path(dir, &req.game_id);
        if let Some(pixmap) = read_raw_cache(&raw_cached) {
            if deliver(tx, req.game_id, pixmap).is_err() {
                return;
            }
            continue;
        }

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
        let decoded = crop_to_aspect(decoded, TARGET_ART_ASPECT);
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
        write_raw_cache(&raw_cached, &pixmap);
        if deliver(tx, req.game_id, pixmap).is_err() {
            return;
        }
    }
}
