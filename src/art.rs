//! Background cover-art fetch + decode. `library::fetch_art` gets raw JPEG/PNG
//! bytes over the same mTLS-pinned management API `library::fetch_games` uses;
//! decoding (via the pure-Rust `image` crate — no on-device libjpeg/libpng needed)
//! happens on a dedicated thread so a slow/large library never blocks the UI loop.
//! SDL2 `Texture`s aren't `Send` (they borrow a `TextureCreator` tied to the main
//! thread's GL/window context), so this thread only ever produces raw RGBA pixels —
//! `main.rs`'s render loop turns those into textures.
use std::sync::mpsc::{Receiver, Sender};

use crate::library::GameEntry;

/// One decoded cover, ready to become an SDL2 texture on the main thread.
pub struct ArtLoaded {
    pub game_id: String,
    pub width: u32,
    pub height: u32,
    /// Tightly-packed RGBA8, row-major, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
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

fn fetch_all(
    host: &str,
    mgmt_port: u16,
    identity: &(String, String),
    fingerprint: Option<[u8; 32]>,
    games: &[GameEntry],
    tx: &Sender<ArtLoaded>,
) {
    for game in games {
        let Some(path) = game.art.portrait.as_deref().or(game.art.header.as_deref()) else {
            continue;
        };
        let Ok(bytes) = crate::library::fetch_art(host, mgmt_port, identity, fingerprint, path) else {
            continue;
        };
        let Ok(decoded) = image::load_from_memory(&bytes) else {
            continue;
        };
        let rgba = decoded.to_rgba8();
        let (width, height) = rgba.dimensions();
        // A receiver drop (host switched again before this batch finished) just
        // ends the thread early — nothing left to deliver to.
        if tx.send(ArtLoaded { game_id: game.id.clone(), width, height, rgba: rgba.into_raw() }).is_err() {
            return;
        }
    }
}
