//! On-demand cover-art loading with disk cache (not all-at-once, which caused OOM).
//! Fetches via mTLS, decodes with pure-Rust `image` crate, handed to UI as `Pixmap`.
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};

use crate::services::library::GameEntry;
use crate::ui::premultiply_rgba;

/// One decoded cover, ready to composite straight into the UI's frame `Painter`.
pub struct ArtLoaded {
    pub game_id: String,
    pub pixmap: Pixmap,
}

/// A cover the UI wants soon.
struct ArtRequest {
    game_id: String,
    /// Candidate art paths (host-relative or external URL), tried in order — a
    /// host-reported path can 404 even when another variant works fine.
    paths: Vec<String>,
}

/// Max decoded dimension (panel can't show oversized art anyway).
const MAX_ART_DIMENSION: u32 = 480;
/// Grid card portrait aspect (cropped to avoid distortion).
const TARGET_ART_ASPECT: f32 = 3.0 / 4.0;

/// Center-crop to aspect ratio (no-op if already close).
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

/// Cache version magic ("PFR2" — bumped for center-cropped art).
const RAW_CACHE_MAGIC: u32 = 0x50465232;

fn cache_root() -> PathBuf {
    let home = std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from);
    home.join("art-cache")
}

fn cache_name(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn cache_dir(host: &str, port: u16) -> PathBuf {
    cache_root().join(cache_name(&format!("{host}_{port}")))
}

/// Clear a forgotten host's cached art (best-effort).
pub fn clear_host_cache(host: &str, port: u16) {
    let _ = std::fs::remove_dir_all(cache_dir(host, port));
}

fn raw_cache_path(dir: &std::path::Path, game_id: &str) -> PathBuf {
    dir.join(format!("{}.raw", cache_name(game_id)))
}

/// Write-then-rename (prevents truncated cache files on kill mid-write).
fn write_raw_cache(path: &std::path::Path, pixmap: &Pixmap) {
    let mut buf = Vec::with_capacity(12 + pixmap.data().len());
    buf.extend_from_slice(&RAW_CACHE_MAGIC.to_le_bytes());
    buf.extend_from_slice(&pixmap.width().to_le_bytes());
    buf.extend_from_slice(&pixmap.height().to_le_bytes());
    buf.extend_from_slice(pixmap.data());
    let tmp = path.with_extension("raw.tmp");
    if std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Read raw cache, if present and well-formed.
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

/// Stretch to card size (done here, not in each card build, to save armv7 cost).
fn resize_pixmap(src: &Pixmap, w: u32, h: u32) -> Option<Pixmap> {
    let mut dst = Pixmap::new(w, h)?;
    let (sw, sh) = (src.width() as f32, src.height() as f32);
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let transform = Transform::from_scale(w as f32 / sw, h as f32 / sh);
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    dst.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    Some(dst)
}

/// `worker`'s fixed, per-host config — bundled to keep its arg count sane.
struct WorkerConfig {
    host: String,
    mgmt_port: u16,
    identity: (String, String),
    fingerprint: Option<[u8; 32]>,
    dir: PathBuf,
    card_w: u32,
    card_h: u32,
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
    /// Spawn loader. `mgmt_port` is what's dialed (separate from identity `port`).
    /// Card dimensions determine cover stretch-to size.
    pub fn spawn(
        host: String,
        port: u16,
        mgmt_port: u16,
        identity: (String, String),
        fingerprint: Option<[u8; 32]>,
        card_w: u32,
        card_h: u32,
    ) -> Self {
        let (tx_req, rx_req) = std::sync::mpsc::channel::<ArtRequest>();
        let (tx_done, rx_done) = std::sync::mpsc::channel::<ArtLoaded>();
        let dir = cache_dir(&host, port);
        let config = WorkerConfig {
            host,
            mgmt_port,
            identity,
            fingerprint,
            dir,
            card_w,
            card_h,
        };
        std::thread::Builder::new()
            .name("punktfunk-webos-art".into())
            .spawn(move || worker(&config, &rx_req, &tx_done))
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
        // Preference order: portrait (right aspect), then header, then hero.
        let paths: Vec<String> = [
            game.art.portrait.as_deref(),
            game.art.header.as_deref(),
            game.art.hero.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect();
        if paths.is_empty() {
            return;
        }
        // A closed channel means the worker is gone; the card keeps its placeholder.
        let _ = self.tx.send(ArtRequest {
            game_id: game.id.clone(),
            paths,
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

fn worker(config: &WorkerConfig, rx: &Receiver<ArtRequest>, tx: &Sender<ArtLoaded>) {
    let WorkerConfig {
        host,
        mgmt_port,
        identity,
        fingerprint,
        dir,
        card_w,
        card_h,
    } = config;
    let (host, mgmt_port, fingerprint, card_w, card_h) = (host.as_str(), *mgmt_port, *fingerprint, *card_w, *card_h);
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
            let sized = resize_pixmap(&pixmap, card_w, card_h).unwrap_or(pixmap);
            if deliver(tx, req.game_id, sized).is_err() {
                return;
            }
            continue;
        }

        let cached = dir.join(cache_name(&req.game_id));
        let bytes = match std::fs::read(&cached) {
            Ok(b) if !b.is_empty() => b,
            _ => {
                if agent.is_none() {
                    match crate::services::library::agent(identity, fingerprint) {
                        Ok(a) => agent = Some(a),
                        Err(e) => {
                            tracing::warn!("art: {} building mTLS agent failed: {e}", req.game_id);
                            continue;
                        }
                    }
                }
                let Some(a) = agent.as_ref() else { continue };
                let mut fetched = None;
                for path in &req.paths {
                    match crate::services::library::fetch_art(a, host, mgmt_port, path) {
                        Ok(b) => {
                            fetched = Some(b);
                            break;
                        }
                        Err(e) => tracing::warn!("art: {} fetch {} failed: {e}", req.game_id, path),
                    }
                }
                let Some(fetched) = fetched else {
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

        let decoded = match image::load_from_memory(&bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("art: {} decode failed ({} bytes): {e}", req.game_id, bytes.len());
                // Drop a cache entry that won't decode — otherwise it poisons this card for
                // the life of the install.
                let _ = std::fs::remove_file(&cached);
                continue;
            }
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
            tracing::warn!("art: {} decoded to zero size ({width}x{height})", req.game_id);
            continue;
        };
        let mut buf = rgba.into_raw();
        premultiply_rgba(&mut buf);
        let Some(pixmap) = Pixmap::from_vec(buf, size) else {
            tracing::warn!("art: {} Pixmap::from_vec failed ({width}x{height})", req.game_id);
            continue;
        };
        write_raw_cache(&raw_cached, &pixmap);
        let sized = resize_pixmap(&pixmap, card_w, card_h).unwrap_or(pixmap);
        if deliver(tx, req.game_id, sized).is_err() {
            return;
        }
    }
}
