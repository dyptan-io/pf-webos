//! Anti-aliased software rendering backend (`tiny_skia` Pixmap framebuffer).
use crate::ui::render::Color;
use crate::ui::render::Rect;
use std::cell::RefCell;
use std::collections::HashMap;
use tiny_skia::{
    Color as SkColor, FillRule, FilterQuality, IntSize, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform,
};

pub fn sk_color(c: Color) -> SkColor {
    SkColor::from_rgba8(c.r, c.g, c.b, c.a)
}

/// Flat-color paint (no gradients/patterns). Anti-aliasing off for cheaper scan-conversion (~15-25% faster).
pub fn solid_paint(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(sk_color(color));
    paint.anti_alias = false;
    paint
}

/// Rounded-rect as Bezier path (`tiny_skia` has no built-in); falls back to plain rect if radius ~0.
pub fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<tiny_skia::Path> {
    const K: f32 = 0.552_284_7;

    let r = radius.max(0.0).min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    if r < 0.5 {
        pb.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
        return pb.finish();
    }
    let k = K * r;
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// One frame's whole-screen framebuffer. `App::render` draws every screen into a
/// single `Painter`; `main.rs` uploads the result to one SDL2 texture and presents
/// it, rather than issuing a texture copy per widget as the old canvas-based
/// version did.
pub struct Painter {
    pixmap: Pixmap,
    /// Drawing offset: every coordinate a caller passes is shifted by `-origin`
    /// before it hits the buffer. Lets a painter whose buffer covers only a
    /// sub-region of the screen still be fed absolute, screen-space geometry — the
    /// modal tile uses this so it can be sized to just the card's bounding box
    /// (a fraction of the full-screen raster + GPU upload it used to be) while the
    /// per-screen render fns keep computing centered, absolute card rects. `(0, 0)`
    /// for every full-screen painter, so their behaviour is unchanged.
    origin: (i32, i32),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShadowKey {
    w: u32,
    h: u32,
    radius: i32,
    blur_bits: u32,
    opacity: u8,
}

thread_local! {
    /// Rendered (padded, box-blurred) shadow shapes, keyed by the params that fully
    /// determine their pixels — shared process-wide, not a `Painter` field: every
    /// grid card gets its own fresh `Painter` (`render_card_tile` calls
    /// `Painter::new` per card), so a cache on `Painter` itself would never hit past
    /// the first card of a build. `thread_local` (not a plain `static`) is safe here
    /// without `unsafe`/atomics since every `Painter` is built on the single
    /// SDL/render thread.
    static SHADOW_CACHE: RefCell<HashMap<ShadowKey, Pixmap>> = RefCell::new(HashMap::new());
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlowKey {
    w: u32,
    h: u32,
    radius: i32,
    blur_bits: u32,
    color: u32,
}

thread_local! {
    /// Same cached-shape technique as `SHADOW_CACHE`, for the focused-card glow
    /// (`fill_glow`) — one blurred colored shape shared by every card of the same
    /// size, since focus only ever moves, it never changes card dimensions.
    static GLOW_CACHE: RefCell<HashMap<GlowKey, Pixmap>> = RefCell::new(HashMap::new());
}

impl Painter {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pixmap: Pixmap::new(width.max(1), height.max(1)).expect("nonzero framebuffer size"),
            origin: (0, 0),
        }
    }

    /// Shifts all subsequent drawing so `(x, y)` in caller (screen) space maps to
    /// this buffer's top-left — see the `origin` field. Set once, right after
    /// `new`, for a sub-region tile.
    pub fn set_origin(&mut self, x: i32, y: i32) {
        self.origin = (x, y);
    }

    fn off(&self, rect: Rect) -> Rect {
        rect.offset(-self.origin.0, -self.origin.1)
    }

    /// Raw premultiplied RGBA8 bytes, row-major, `width() * height() * 4` long —
    /// the exact byte order `sdl2::pixels::PixelFormatEnum::RGBA32` expects, so
    /// `main.rs` can upload it to an SDL2 texture with no further conversion (every
    /// frame starts with an opaque `clear`, so alpha is 255 everywhere by the time
    /// this is read — premultiplied and straight are then identical).
    pub fn data(&self) -> &[u8] {
        self.pixmap.data()
    }

    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.fill_rounded_rect(rect, 0, color);
    }

    /// Fills the whole pixmap with `color` under a vertical alpha ramp running from
    /// `top_alpha` to `bottom_alpha` — the scroll-fade tiles (see
    /// [`super::render_scroll_fade_tile`]). Reversing the two arguments gives the mirrored
    /// ramp the top edge needs.
    ///
    /// Written straight into the buffer rather than through `fill_rect` per row: a
    /// scanline-at-a-time gradient is exactly the "many small fills" shape that measured
    /// ~300ms full-screen on this hardware (docs/NOTES.md), and going through the rasterizer buys
    /// nothing for axis-aligned solid rows.
    ///
    /// Eased with smoothstep rather than a straight line or a square. Linear puts visible
    /// tint across the whole band; squaring (the first attempt) held so much of the band near
    /// clear that the fade barely read at all. Smoothstep is symmetric — half-strength at the
    /// midpoint — with both ends easing out, so neither edge shows a seam.
    pub fn fill_vertical_fade(&mut self, color: Color, top_alpha: u8, bottom_alpha: u8) {
        let (w, h) = (self.pixmap.width(), self.pixmap.height());
        let last_row = f32::from(u16::try_from(h.saturating_sub(1)).unwrap_or(u16::MAX)).max(1.0);
        let row_bytes = w as usize * 4;
        let (from, to) = (f32::from(top_alpha), f32::from(bottom_alpha));
        for (y, row) in self.pixmap.data_mut().chunks_exact_mut(row_bytes).enumerate() {
            let t = (y as f32 / last_row).clamp(0.0, 1.0);
            let eased = t * t * (3.0 - 2.0 * t);
            let alpha = (from + (to - from) * eased).round().clamp(0.0, 255.0) as u8;
            // Premultiplied, because that is what a tile's buffer holds — `Compositor::upload`
            // un-premultiplies on the way to the GPU (docs/NOTES.md). Writing straight alpha
            // here would show up as a fade that washes toward white at its dense end.
            // Rounded, not truncated: flooring biases every channel down by a
            // fraction of a level, and over the card the fade must reconstruct
            // SIDEBAR_BG exactly or it bands as a dark rectangle on OLED near-black.
            let premul = |c: u8| (((u16::from(c) * u16::from(alpha)) + 127) / 255) as u8;
            let px = [premul(color.r), premul(color.g), premul(color.b), alpha];
            for pixel in row.chunks_exact_mut(4) {
                pixel.copy_from_slice(&px);
            }
        }
    }

    pub fn fill_rounded_rect(&mut self, rect: Rect, radius: i32, color: Color) {
        let rect = self.off(rect);
        let (w, h) = (rect.width() as f32, rect.height() as f32);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let Some(path) = rounded_rect_path(rect.x() as f32, rect.y() as f32, w, h, radius as f32) else {
            return;
        };
        self.fill(&path, color);
    }

    pub fn stroke_rounded_rect(&mut self, rect: Rect, radius: i32, color: Color, width: f32) {
        let rect = self.off(rect);
        let (w, h) = (rect.width() as f32, rect.height() as f32);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let Some(path) = rounded_rect_path(rect.x() as f32, rect.y() as f32, w, h, radius as f32) else {
            return;
        };
        let paint = solid_paint(color);
        let stroke = Stroke {
            width,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, color: Color) {
        if r <= 0.0 {
            return;
        }
        let (cx, cy) = (cx - self.origin.0 as f32, cy - self.origin.1 as f32);
        let Some(path) = PathBuilder::from_circle(cx, cy, r) else {
            return;
        };
        self.fill(&path, color);
    }

    fn fill(&mut self, path: &tiny_skia::Path, color: Color) {
        let paint = solid_paint(color);
        self.pixmap
            .fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    /// A soft, real (box-blurred) drop shadow for a rounded-rect shape, offset by
    /// `(dx, dy)` — replaces the old flat single-offset hard-edged rect, which had
    /// no actual softness to sell "shadow" at TV viewing distance.
    ///
    /// The blurred shape only depends on `(rect.width(), rect.height(), radius,
    /// blur, opacity)`, not position — every card of the same size/style (the
    /// whole game grid, every sidebar row) reuses one cached shape instead of
    /// re-running the box blur per card per frame.
    pub fn fill_shadow(&mut self, rect: Rect, radius: i32, dx: f32, dy: f32, blur: f32, opacity: u8) {
        if rect.width() == 0 || rect.height() == 0 {
            return;
        }
        let pad = blur.ceil().max(0.0) as i32 + 1;
        let (ox, oy) = self.origin;
        let key = ShadowKey {
            w: rect.width(),
            h: rect.height(),
            radius,
            blur_bits: blur.to_bits(),
            opacity,
        };
        SHADOW_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let shape = match cache.entry(key) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let Some(shape) = render_shadow_shape(rect.width(), rect.height(), radius, pad, blur, opacity)
                    else {
                        return;
                    };
                    e.insert(shape)
                }
            };
            self.pixmap.draw_pixmap(
                rect.x() - pad + dx.round() as i32 - ox,
                rect.y() - pad + dy.round() as i32 - oy,
                shape.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        });
    }

    /// A soft colored glow centered on a rounded-rect shape — same cached-shape,
    /// box-blurred technique as `fill_shadow`, just tinted and un-offset (a glow
    /// surrounds its shape evenly; a shadow falls to one side).
    pub fn fill_glow(&mut self, rect: Rect, radius: i32, color: Color, blur: f32) {
        if rect.width() == 0 || rect.height() == 0 {
            return;
        }
        let pad = blur.ceil().max(0.0) as i32 + 1;
        let (ox, oy) = self.origin;
        let key = GlowKey {
            w: rect.width(),
            h: rect.height(),
            radius,
            blur_bits: blur.to_bits(),
            color: u32::from_be_bytes([color.r, color.g, color.b, color.a]),
        };
        GLOW_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let shape = match cache.entry(key) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let Some(shape) = render_glow_shape(rect.width(), rect.height(), radius, pad, blur, color) else {
                        return;
                    };
                    e.insert(shape)
                }
            };
            self.pixmap.draw_pixmap(
                rect.x() - pad - ox,
                rect.y() - pad - oy,
                shape.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        });
    }

    /// Blurs the pixmap content already drawn within `rect`, in place (clamped to bounds).
    pub fn blur_rect(&mut self, rect: Rect, radius: usize) {
        if radius == 0 {
            return;
        }
        let rect = self.off(rect);
        let (pw, ph) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        let x0 = rect.x().max(0);
        let y0 = rect.y().max(0);
        let x1 = (rect.x() + rect.width() as i32).min(pw);
        let y1 = (rect.y() + rect.height() as i32).min(ph);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let row_bytes = w * 4;
        let src_stride = pw as usize * 4;
        let data = self.pixmap.data_mut();

        let mut region = vec![0u8; row_bytes * h];
        for y in 0..h {
            let src = (y0 as usize + y) * src_stride + x0 as usize * 4;
            region[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(&data[src..src + row_bytes]);
        }

        let mut scratch = vec![0u8; w * h];
        for channel in 0..4 {
            box_blur_channel(&mut region, &mut scratch, w, h, channel, radius);
        }

        for y in 0..h {
            let dst = (y0 as usize + y) * src_stride + x0 as usize * 4;
            data[dst..dst + row_bytes].copy_from_slice(&region[y * row_bytes..(y + 1) * row_bytes]);
        }
    }

    /// Frosted-glass panel: blurs whatever's under `rect`, then tints it.
    pub fn fill_frosted_rect(&mut self, rect: Rect, radius: i32, tint: Color, blur_radius: usize) {
        self.blur_rect(rect, blur_radius);
        self.fill_rounded_rect(rect, radius, tint);
    }

    pub fn draw_pixmap(&mut self, x: i32, y: i32, src: &Pixmap) {
        self.pixmap.draw_pixmap(
            x - self.origin.0,
            y - self.origin.1,
            src.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }

    /// Composites `src` scaled to exactly fill `dst` — the one caller is game-art
    /// rendering (`ui::draw_poster_card`), and only at tile-build time (see
    /// `App::prepare_tiles`), not per frame: the result is cached into the card's tile
    /// and only re-scaled when the art or card size actually changes. `Bilinear`
    /// (rather than `Nearest`) is worth its modest per-call cost here since it's paid
    /// once per card build, not every frame — plain `Nearest` scaling left visible
    /// jaggies on art whose source resolution didn't cleanly divide into the card size.
    pub fn draw_pixmap_scaled(&mut self, dst: Rect, src: &Pixmap) {
        let dst = self.off(dst);
        let (dw, dh) = (dst.width() as f32, dst.height() as f32);
        let (sw, sh) = (src.width() as f32, src.height() as f32);
        if dw <= 0.0 || dh <= 0.0 || sw <= 0.0 || sh <= 0.0 {
            return;
        }
        let transform = Transform::from_scale(dw / sw, dh / sh).post_translate(dst.x() as f32, dst.y() as f32);
        let paint = PixmapPaint {
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        self.pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    }
}

/// How far a shadow's blur extends past the shape casting it, in px — a fixed
/// constant (not derived from anything) picked to read as a soft TV-scale shadow.
pub const SHADOW_BLUR: f32 = 14.0;

/// Rasterizes a `(w, h)` rounded-rect shape into a small padded alpha buffer and
/// box-blurs it (3 passes — a cheap approximation of a Gaussian blur, good enough
/// at TV viewing distance for a drop shadow), returning the standalone shadow
/// shape as a black, premultiplied `Pixmap` ready to be composited at any
/// position — see `Painter::fill_shadow`'s cache, keyed on everything that
/// determines these pixels (size/radius/blur/opacity, not position).
pub fn render_shadow_shape(w: u32, h: u32, radius: i32, pad: i32, blur: f32, opacity: u8) -> Option<Pixmap> {
    let (pw, ph) = (w as i32 + 2 * pad, h as i32 + 2 * pad);
    if pw <= 0 || ph <= 0 {
        return None;
    }
    let mut shape = Pixmap::new(pw as u32, ph as u32)?;
    let path = rounded_rect_path(pad as f32, pad as f32, w as f32, h as f32, radius as f32)?;
    let paint = solid_paint(Color::RGBA(0, 0, 0, opacity));
    shape.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);

    // tiny-skia stores premultiplied RGBA; a pure-black shape's R/G/B channels are
    // always 0, so its alpha channel alone fully describes the shape — blur that
    // channel directly rather than blurring all 4 for no visual difference.
    let mut alpha: Vec<u8> = shape.data().iter().skip(3).step_by(4).copied().collect();
    let radius_px = (blur / 2.0).round().max(1.0) as usize;
    for _ in 0..3 {
        box_blur(&mut alpha, pw as usize, ph as usize, radius_px);
    }
    for (i, a) in alpha.into_iter().enumerate() {
        shape.data_mut()[i * 4 + 3] = a; // R/G/B stay 0 (premultiplied black)
    }

    Some(shape)
}

/// Rasterizes a `(w, h)` rounded-rect shape filled with `color` and box-blurs it
/// on all four channels (unlike `render_shadow_shape`, a colored glow can't
/// reduce to a single alpha channel) — see `Painter::fill_glow`'s cache, keyed
/// on everything that determines these pixels.
pub fn render_glow_shape(w: u32, h: u32, radius: i32, pad: i32, blur: f32, color: Color) -> Option<Pixmap> {
    let (pw, ph) = (w as i32 + 2 * pad, h as i32 + 2 * pad);
    if pw <= 0 || ph <= 0 {
        return None;
    }
    let mut shape = Pixmap::new(pw as u32, ph as u32)?;
    let path = rounded_rect_path(pad as f32, pad as f32, w as f32, h as f32, radius as f32)?;
    shape.fill_path(
        &path,
        &solid_paint(color),
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let (pwu, phu) = (pw as usize, ph as usize);
    let mut data = shape.data().to_vec();
    let mut scratch = vec![0u8; pwu * phu];
    let radius_px = (blur / 2.0).round().max(1.0) as usize;
    for _ in 0..3 {
        for channel in 0..4 {
            box_blur_channel(&mut data, &mut scratch, pwu, phu, channel, radius_px);
        }
    }
    Pixmap::from_vec(data, IntSize::from_wh(pw as u32, ph as u32)?)
}

/// Separable box blur (horizontal pass into `tmp`, then vertical back into
/// `pixels`) — both passes are the same 1D sliding-window average, just walking
/// the buffer in a different direction (see `blur_1d`).
pub fn box_blur(pixels: &mut [u8], w: usize, h: usize, radius: usize) {
    if radius == 0 {
        return;
    }
    let mut tmp = vec![0u8; pixels.len()];
    for y in 0..h {
        blur_1d(w, radius, |x| pixels[y * w + x], |x, v| tmp[y * w + x] = v);
    }
    for x in 0..w {
        blur_1d(h, radius, |y| tmp[y * w + x], |y, v| pixels[y * w + x] = v);
    }
}

/// `box_blur`, but for one channel of a packed RGBA buffer, with a caller-supplied scratch buffer.
pub fn box_blur_channel(region: &mut [u8], scratch: &mut [u8], w: usize, h: usize, channel: usize, radius: usize) {
    if radius == 0 {
        return;
    }
    for y in 0..h {
        blur_1d(
            w,
            radius,
            |x| region[(y * w + x) * 4 + channel],
            |x, v| scratch[y * w + x] = v,
        );
    }
    for x in 0..w {
        blur_1d(
            h,
            radius,
            |y| scratch[y * w + x],
            |y, v| region[(y * w + x) * 4 + channel] = v,
        );
    }
}

/// A 1D sliding-window average over `len` samples (read/written through the given
/// accessors, so the same core serves both a blur's horizontal and vertical
/// passes), via a prefix sum so each output sample is O(1) regardless of `radius`.
pub fn blur_1d(len: usize, radius: usize, read: impl Fn(usize) -> u8, mut write: impl FnMut(usize, u8)) {
    let mut prefix = vec![0u32; len + 1];
    for i in 0..len {
        prefix[i + 1] = prefix[i] + u32::from(read(i));
    }
    for i in 0..len {
        let lo = i.saturating_sub(radius);
        let hi = (i + radius).min(len - 1);
        let count = (hi - lo + 1) as u32;
        write(i, ((prefix[hi + 1] - prefix[lo]) / count) as u8);
    }
}

/// `tiny-skia` stores premultiplied alpha; `SDL2_ttf`'s `.blended()` glyph surfaces
/// and `image`'s decoded covers are both straight alpha — every raw-RGBA buffer
/// feeding a `Pixmap` (see `pixmap_from_ttf_surface`, `art.rs`) goes through this
/// first.
pub fn premultiply_rgba(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        px[0] = ((u32::from(px[0]) * a) / 255) as u8;
        px[1] = ((u32::from(px[1]) * a) / 255) as u8;
        px[2] = ((u32::from(px[2]) * a) / 255) as u8;
    }
}
