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
