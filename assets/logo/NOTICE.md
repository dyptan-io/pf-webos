# punktfunk logo

`punktfunk-logo-dark.svg` is the brand's actual logo artwork (dark / no-border
variant, verbatim from the punktfunk design exports). `logo-sidebar.png` is its
raster for the sidebar lockup, embedded via `include_bytes!` in `src/ui.rs`
(`logo_pixmap`). Regenerate after an artwork change with:

```sh
# strip the white export-canvas <rect>, then rasterize at the display size
rsvg-convert -w 190 --keep-aspect-ratio logo_full.svg -o logo-sidebar.png
```

`packaging/splash.png` is generated from the same artwork (mark only, tight
viewBox, centered on the brand-dark `#1c1530` 1920x1080 canvas).

`punktfunk-spinner.gif` is the grid's loading-spinner animation (exported from
lottiefiles.com's free "Purple Spinner" animation,
<https://lottiefiles.com/free-animation/purple-spinner-peYjszu1K5> — confirm
that animation's current license terms before shipping a build with it).
Decoded into frame data at build time by `build.rs`'s `build_spinner_frames`
(see `ui/tiles.rs`'s `spinner_frames`) — no separate regen step needed, just
replace the GIF and rebuild.
