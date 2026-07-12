# MaterialIcons-subset.ttf

Subsetted from Google's [Material Icons](https://github.com/google/material-design-icons)
font (`font/MaterialIcons-Regular.ttf`), licensed under the Apache License, Version 2.0
(full text in `LICENSE`, this directory).

Subsetted with `fonttools`' `pyftsubset` down to only the glyphs `ui.rs` actually draws —
the full font is ~357 KB covering 2000+ icons; this subset is ~1.7 KB:

| Icon (`ui::icon_font` constant) | Material Icons name   | Codepoint |
|----------------------------------|-----------------------|-----------|
| `ICON_TV`                        | `tv`                  | `U+E333`  |
| `ICON_LOCK`                       | `lock`                | `U+E897`  |
| `ICON_ADD`                        | `add`                 | `U+E145`  |
| `ICON_CLOSE`                      | `close`               | `U+E5CD`  |
| `ICON_SETTINGS`                   | `settings`            | `U+E8B8`  |
| `ICON_MONITOR`                    | `monitor`             | `U+EF5B`  |
| `ICON_SCHEDULE`                   | `schedule`            | `U+E8B5`  |
| `ICON_SIGNAL`                     | `signal_cellular_alt` | `U+E202`  |
| `ICON_SUN`                        | `wb_sunny`            | `U+E430`  |
| `ICON_CHEVRON_DOWN`               | `arrow_drop_down`     | `U+E5C5`  |

To regenerate after adding/changing an icon, re-run against a fresh copy of the upstream
font with the updated codepoint list:

```
pyftsubset MaterialIcons-Regular.ttf \
  --unicodes=U+E333,U+E897,U+E145,U+E5CD,U+E8B8,U+E8B5,U+E202,U+E430,U+E5C5,U+EF5B \
  --output-file=MaterialIcons-subset.ttf \
  --no-hinting --desubroutinize --name-IDs="" --notdef-glyph --notdef-outline
```
