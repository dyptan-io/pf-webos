//! On-demand cover-art loading with disk cache (not all-at-once, which caused OOM).
//! Fetches via mTLS, decodes with pure-Rust `image` crate, handed to UI as `Pixmap`.
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};

use crate::services::library::GameEntry;
use crate::ui::premultiply_rgba;

/// A decoded wide hero image, straight (not premultiplied) RGBA8 — it goes to the
/// GPU as a raw texture (`Compositor::upload_raw`) rather than through a `Painter`,
/// since nothing is ever rasterized on top of it.
pub struct HeroImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// One decoded image, ready to composite.
pub enum ArtLoaded {
    /// Grid cover, stretched to card size.
    Card { game_id: String, pixmap: Pixmap },
    /// Wide art for the connecting screen.
    Hero { game_id: String, image: HeroImage },
}

/// Which variant of a game's art a request wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArtKind {
    Card,
    Hero,
}

/// Number of `ArtKind` variants — the width of `ArtLoader::requested`.
const ART_KINDS: usize = 2;

/// An image the UI wants soon.
struct ArtRequest {
    game_id: String,
    kind: ArtKind,
    /// Candidate art paths (host-relative or external URL), tried in order — a
    /// host-reported path can 404 even when another variant works fine.
    paths: Vec<String>,
}

/// Max decoded dimension (panel can't show oversized art anyway).
const MAX_ART_DIMENSION: u32 = 480;
/// Grid card portrait aspect (cropped to avoid distortion).
const TARGET_ART_ASPECT: f32 = 3.0 / 4.0;
/// Max decoded hero width. Full-screen art, so far larger than a card — but deliberately
/// under 1080p width and left for the GPU to upscale: it is a dimmed, moving backdrop, and
/// every extra pixel is resize time on the launch path plus memory for one transient
/// screen. Source heroes are often 3840 wide, so this is where most of the cost is.
const MAX_HERO_WIDTH: u32 = 1280;
/// Hero crop aspect. Deliberately wider than any panel (16:9 at its widest), because
/// the slack between the two is exactly what the connecting screen's pan travels.
const HERO_ASPECT: f32 = 2.4;

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
/// Hero decoded-pixel cache magic ("PFH1").
const HERO_CACHE_MAGIC: u32 = 0x50464831;
/// Filename suffixes that mark a hero's two cache files — see [`cache_class`].
const HERO_RAW_SUFFIX: &str = ".hero.raw";
const HERO_BYTES_SUFFIX: &str = ".hero";

/// Per-host disk budget for card art (encoded bytes plus the decoded `.raw`). A cover caps at
/// 360×480 straight RGBA, so ~0.7 MB decoded each: an unbounded cache is ~140 MB for a
/// 200-game library, on a TV with no disk to spare. At this budget roughly 80 covers stay
/// resident, which is far more than the grid window, and a miss costs one LAN fetch.
const CARD_CACHE_BUDGET: u64 = 56 * 1024 * 1024;
/// Per-host disk budget for hero art. ~2.7 MB per decoded hero at [`MAX_HERO_WIDTH`], so this
/// keeps the last ~8 launched. Its own budget rather than a share of the card one because the
/// two compete on nothing: a full grid of covers must not evict the hero of the game being
/// launched, and one launch must not evict the visible grid.
const HERO_CACHE_BUDGET: u64 = 24 * 1024 * 1024;
/// Card writes between prunes. Pruning walks the host's directory, so doing it per cover would
/// put a `read_dir` on every grid fetch; heroes are few and large enough to prune every time.
const CARD_PRUNE_EVERY: u32 = 24;

fn cache_root() -> PathBuf {
    crate::services::paths::app_dir().join("art-cache")
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

/// Decoded-pixel cache path. Heroes get their own name so a game can cache both.
fn raw_cache_path(dir: &std::path::Path, game_id: &str, kind: ArtKind) -> PathBuf {
    match kind {
        ArtKind::Card => dir.join(format!("{}.raw", cache_name(game_id))),
        ArtKind::Hero => dir.join(format!("{}{HERO_RAW_SUFFIX}", cache_name(game_id))),
    }
}

/// Encoded-bytes cache path (what the host served, undecoded).
fn bytes_cache_path(dir: &std::path::Path, game_id: &str, kind: ArtKind) -> PathBuf {
    match kind {
        ArtKind::Card => dir.join(cache_name(game_id)),
        ArtKind::Hero => dir.join(format!("{}{HERO_BYTES_SUFFIX}", cache_name(game_id))),
    }
}

/// Write-then-rename (prevents truncated cache files on kill mid-write). Header and pixels go
/// as two writes so a full-image copy isn't made just to prepend 12 bytes. Best-effort: a cache
/// that can't be written costs a re-fetch, nothing more.
fn write_raw(path: &std::path::Path, magic: u32, width: u32, height: u32, pixels: &[u8]) {
    let mut header = [0u8; 12];
    header[0..4].copy_from_slice(&magic.to_le_bytes());
    header[4..8].copy_from_slice(&width.to_le_bytes());
    header[8..12].copy_from_slice(&height.to_le_bytes());
    let _ = crate::services::store::write_atomic_parts(path, &[&header, pixels], "art raw cache");
}

/// Read raw cache, if present and written with this magic (and so this pixel convention —
/// card pixels are premultiplied, hero pixels straight, and the two must never be
/// mistaken for each other).
fn read_raw(path: &std::path::Path, magic: u32) -> Option<(u32, u32, Vec<u8>)> {
    let mut buf = std::fs::read(path).ok()?;
    if u32::from_le_bytes(buf.get(0..4)?.try_into().ok()?) != magic {
        return None;
    }
    let width = u32::from_le_bytes(buf.get(4..8)?.try_into().ok()?);
    let height = u32::from_le_bytes(buf.get(8..12)?.try_into().ok()?);
    if buf.len() - 12 != width as usize * height as usize * 4 {
        return None;
    }
    // Shift the payload down over the header rather than copying it out: the pixels are the
    // whole file, and this way the read's allocation is the one handed on.
    buf.drain(..12);
    Some((width, height, buf))
}

/// Which budget a cached file counts against. Heroes are matched first: `x.hero.raw` ends with
/// both suffixes. `None` for a staging file, which no budget owns — a `.tmp` left behind by a
/// kill mid-write is deleted outright by [`prune_cache`].
fn cache_class(path: &std::path::Path) -> Option<ArtKind> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.ends_with(".tmp") {
        return None;
    }
    if name.ends_with(HERO_RAW_SUFFIX) || name.ends_with(HERO_BYTES_SUFFIX) {
        return Some(ArtKind::Hero);
    }
    Some(ArtKind::Card)
}

/// Holds one host's cache inside its per-kind budget, oldest file first.
///
/// The quota is per host directory, so a host with a huge library cannot evict another host's
/// art — and forgetting a host (`clear_host_cache`) reclaims exactly its own share.
///
/// Eviction is by write time, not use time: nothing here touches a file it serves from cache, and
/// an extra `utimes` per grid card is not worth the sharper policy. So a re-fetch after eviction
/// is possible for art the user is still looking at, which costs one LAN request.
fn prune_cache(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(ArtKind, std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        let Some(kind) = cache_class(&path) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let Ok(meta) = path.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        files.push((kind, modified, meta.len(), path));
    }
    files.sort_by_key(|(_, modified, _, _)| *modified);

    for (kind, budget) in [(ArtKind::Card, CARD_CACHE_BUDGET), (ArtKind::Hero, HERO_CACHE_BUDGET)] {
        let mut total: u64 = files
            .iter()
            .filter(|(k, ..)| *k == kind)
            .map(|(_, _, len, _)| *len)
            .sum();
        if total <= budget {
            continue;
        }
        for (_, _, len, path) in files.iter().filter(|(k, ..)| *k == kind) {
            if total <= budget {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total -= len;
            }
        }
        tracing::debug!("art: pruned {:?} cache in {} to {total} bytes", kind, dir.display());
    }
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
    /// Which protocol's art transport to open — see `backend::ArtFetch`. This module knows
    /// nothing about how a cover is fetched, only that `ArtRequest::paths` are handles the
    /// backend understands.
    backend: &'static dyn crate::backend::HostBackend,
    host: String,
    query_port: u16,
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
    /// Ids already handed to the worker, so scrolling over the same card repeatedly doesn't
    /// queue it repeatedly. Kept per kind (indexed by `ArtKind as usize`) because focus moving
    /// back and forth over a card must not re-queue its much larger hero either, and the two
    /// are forgotten independently.
    requested: [HashSet<String>; ART_KINDS],
}

impl ArtLoader {
    /// Spawn loader. `query_port` is what's dialed (separate from identity `port`).
    /// Card dimensions determine cover stretch-to size.
    pub fn spawn(
        backend: &'static dyn crate::backend::HostBackend,
        host: String,
        port: u16,
        query_port: u16,
        identity: (String, String),
        fingerprint: Option<[u8; 32]>,
        (card_w, card_h): (u32, u32),
    ) -> Self {
        let (tx_req, rx_req) = std::sync::mpsc::channel::<ArtRequest>();
        let (tx_done, rx_done) = std::sync::mpsc::channel::<ArtLoaded>();
        let dir = cache_dir(&host, port);
        let config = WorkerConfig {
            backend,
            host,
            query_port,
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
            requested: Default::default(),
        }
    }

    /// Queues one request unless `game_id` has already been asked for in `kind`. The id is
    /// remembered even when there are no candidate paths: a game with no art at all must not
    /// be re-queued every frame forever.
    ///
    /// `paths` is a closure so the already-requested case — the overwhelmingly common one, since
    /// these are called for the whole prefetch window every frame — builds no path strings at all.
    fn queue(&mut self, game_id: &str, kind: ArtKind, paths: impl FnOnce() -> Vec<String>) {
        let requested = &mut self.requested[kind as usize];
        // Membership before insert: an already-requested id shouldn't pay for an allocation
        // just to be looked up.
        if requested.contains(game_id) {
            return;
        }
        requested.insert(game_id.to_string());
        let paths = paths();
        if paths.is_empty() {
            return;
        }
        // A closed channel means the worker is gone; the card keeps its placeholder.
        let _ = self.tx.send(ArtRequest {
            game_id: game_id.to_string(),
            kind,
            paths,
        });
    }

    /// Asks for `game`'s cover if it hasn't been asked for already. Cheap enough to call
    /// for every card in the prefetch window every frame.
    pub fn request(&mut self, game: &GameEntry) {
        self.queue(&game.id, ArtKind::Card, || {
            // Preference order: portrait (right aspect), then header, then hero.
            [
                game.art.portrait.as_deref(),
                game.art.header.as_deref(),
                game.art.hero.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .collect()
        });
    }

    /// Asks for `game`'s wide hero art (the connecting screen's backdrop) if it hasn't
    /// been asked for already. Called for the focused card, so the image is usually
    /// decoded and waiting by the time the user actually launches.
    ///
    /// Portrait art is deliberately not a fallback here: cropped to a hero's aspect
    /// there'd be almost nothing left of it, and the connecting screen falls back to
    /// its plain black fade perfectly well.
    pub fn request_hero(&mut self, game: &GameEntry) {
        self.queue(&game.id, ArtKind::Hero, || {
            [game.art.hero.as_deref(), game.art.header.as_deref()]
                .into_iter()
                .flatten()
                .map(str::to_string)
                .collect()
        });
    }

    /// Forgets that `game_id`'s hero was requested, so it can be asked for again. Needed
    /// when a hero arrives too late to be of use and is dropped — without this the game
    /// would never get another chance at one, even though its bytes are now cached.
    pub fn forget_hero(&mut self, game_id: &str) {
        self.forget_kind(ArtKind::Hero, game_id);
    }

    /// Forgets that `game_id` was requested, so a later scroll back re-requests it. Served
    /// from the disk cache, so this costs a decode rather than a round-trip.
    pub fn forget(&mut self, game_id: &str) {
        self.forget_kind(ArtKind::Card, game_id);
    }

    fn forget_kind(&mut self, kind: ArtKind, game_id: &str) {
        self.requested[kind as usize].remove(game_id);
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
        backend,
        host,
        query_port,
        identity,
        fingerprint,
        dir,
        card_w,
        card_h,
    } = config;
    let (host, query_port, fingerprint, card_w, card_h) = (host.as_str(), *query_port, *fingerprint, *card_w, *card_h);
    let _ = std::fs::create_dir_all(dir);
    // Once at startup: the budgets are also enforced here, so a directory left over an upgrade
    // (or by a kill before the write-triggered prune below) is brought inside them without
    // waiting for this session to write anything.
    prune_cache(dir);
    // Card writes since the last prune — see `CARD_PRUNE_EVERY`.
    let mut card_writes: u32 = 0;
    // One transport reused for every fetch, so the connection (and, for punktfunk, the
    // client-cert handshake) is paid once rather than per cover — real avoidable cost that
    // scales with library size. Built lazily, so a fully cached library never connects at all.
    let mut fetcher = None;

    // Local queue rather than straight `recv()`: a hero request gates the connecting
    // screen, so it has to jump whatever card-art backlog the grid has just queued. A
    // closed channel means the host was switched away from, and the worker is done.
    let mut queue: VecDeque<ArtRequest> = VecDeque::new();
    loop {
        if queue.is_empty() {
            match rx.recv() {
                Ok(req) => queue.push_back(req),
                Err(_) => return,
            }
        }
        while let Ok(req) = rx.try_recv() {
            queue.push_back(req);
        }
        let at = queue.iter().position(|r| r.kind == ArtKind::Hero).unwrap_or_default();
        let Some(req) = queue.remove(at) else { continue };

        // Decoded-pixel cache. Worth far more for a hero than for a card: decoding a
        // full-size hero JPEG on this SoC takes long enough to miss the launch it was
        // fetched for, so the encoded-bytes layer below is not enough on its own.
        let raw_cached = raw_cache_path(dir, &req.game_id, req.kind);
        let cached_raw = match req.kind {
            ArtKind::Card => read_raw(&raw_cached, RAW_CACHE_MAGIC)
                .and_then(|(width, height, pixels)| Pixmap::from_vec(pixels, IntSize::from_wh(width, height)?))
                .map(|pixmap| {
                    let sized = resize_pixmap(&pixmap, card_w, card_h).unwrap_or(pixmap);
                    ArtLoaded::Card {
                        game_id: req.game_id.clone(),
                        pixmap: sized,
                    }
                }),
            ArtKind::Hero => read_raw(&raw_cached, HERO_CACHE_MAGIC).map(|(width, height, pixels)| ArtLoaded::Hero {
                game_id: req.game_id.clone(),
                image: HeroImage { width, height, pixels },
            }),
        };
        if let Some(loaded) = cached_raw {
            if tx.send(loaded).is_err() {
                return;
            }
            continue;
        }

        let cached = bytes_cache_path(dir, &req.game_id, req.kind);
        let bytes = match std::fs::read(&cached) {
            Ok(b) if !b.is_empty() => b,
            _ => {
                if fetcher.is_none() {
                    match backend.art_fetcher(host, query_port, identity, fingerprint) {
                        Ok(f) => fetcher = Some(f),
                        Err(e) => {
                            tracing::warn!("art: {} opening art transport failed: {e}", req.game_id);
                            continue;
                        }
                    }
                }
                let Some(f) = fetcher.as_ref() else { continue };
                let mut fetched = None;
                for path in &req.paths {
                    match f.fetch(path) {
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
                // otherwise leave a truncated file that gets served from cache forever.
                let _ = crate::services::store::write_atomic_parts(&cached, &[&fetched], "art bytes cache");
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
        let decoded = match req.kind {
            ArtKind::Card => {
                let cropped = crop_to_aspect(decoded, TARGET_ART_ASPECT);
                if cropped.width().max(cropped.height()) > MAX_ART_DIMENSION {
                    cropped.resize(
                        MAX_ART_DIMENSION,
                        MAX_ART_DIMENSION,
                        image::imageops::FilterType::Triangle,
                    )
                } else {
                    cropped
                }
            }
            ArtKind::Hero => {
                let cropped = crop_to_aspect(decoded, HERO_ASPECT);
                if cropped.width() > MAX_HERO_WIDTH {
                    // `u32::MAX` height: `resize` preserves aspect, so width alone bounds it.
                    cropped.resize(MAX_HERO_WIDTH, u32::MAX, image::imageops::FilterType::Triangle)
                } else {
                    cropped
                }
            }
        };
        let rgba = decoded.to_rgba8();
        let (width, height) = rgba.dimensions();
        if width == 0 || height == 0 {
            tracing::warn!("art: {} decoded to zero size ({width}x{height})", req.game_id);
            continue;
        }
        let mut buf = rgba.into_raw();
        let loaded = match req.kind {
            ArtKind::Hero => {
                // Left straight-alpha (no `premultiply_rgba`): it is uploaded as a raw
                // texture, and SDL's blend mode expects straight alpha.
                write_raw(&raw_cached, HERO_CACHE_MAGIC, width, height, &buf);
                prune_cache(dir);
                card_writes = 0;
                ArtLoaded::Hero {
                    game_id: req.game_id,
                    image: HeroImage {
                        width,
                        height,
                        pixels: buf,
                    },
                }
            }
            ArtKind::Card => {
                premultiply_rgba(&mut buf);
                let Some(size) = IntSize::from_wh(width, height) else {
                    continue;
                };
                let Some(pixmap) = Pixmap::from_vec(buf, size) else {
                    tracing::warn!("art: {} Pixmap::from_vec failed ({width}x{height})", req.game_id);
                    continue;
                };
                write_raw(
                    &raw_cached,
                    RAW_CACHE_MAGIC,
                    pixmap.width(),
                    pixmap.height(),
                    pixmap.data(),
                );
                card_writes += 1;
                if card_writes >= CARD_PRUNE_EVERY {
                    prune_cache(dir);
                    card_writes = 0;
                }
                let sized = resize_pixmap(&pixmap, card_w, card_h).unwrap_or(pixmap);
                ArtLoaded::Card {
                    game_id: req.game_id,
                    pixmap: sized,
                }
            }
        };
        if tx.send(loaded).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.hero.raw` ends with `.raw` too, so the order the suffixes are tested in is the whole
    /// of keeping the two budgets apart.
    #[test]
    fn hero_files_are_not_counted_as_card_art() {
        let dir = std::path::Path::new("/cache");
        assert_eq!(cache_class(&dir.join("123.hero.raw")), Some(ArtKind::Hero));
        assert_eq!(cache_class(&dir.join("123.hero")), Some(ArtKind::Hero));
        assert_eq!(cache_class(&dir.join("123.raw")), Some(ArtKind::Card));
        assert_eq!(cache_class(&dir.join("123")), Some(ArtKind::Card));
        assert_eq!(cache_class(&dir.join("123.hero.raw.tmp")), None);
    }

    /// Each host gets its own directory, so one host's library can't crowd out another's.
    #[test]
    fn hosts_get_separate_cache_directories() {
        assert_ne!(cache_dir("10.0.0.2", 47989), cache_dir("10.0.0.3", 47989));
        assert_ne!(cache_dir("10.0.0.2", 47989), cache_dir("10.0.0.2", 47984));
    }
}
