# Geist

punktfunk's brand font — [Vercel's Geist](https://vercel.com/font), licensed under the
SIL Open Font License 1.1 (see `Geist-OFL.txt`).

These OTFs (Regular / Medium / SemiBold / Bold) are copied **verbatim** from the
punktfunk monorepo's `crates/pf-console-ui/assets/fonts/` — the same files every other
punktfunk client bundles — so typography stays identical across clients. Embedded into
the binary via `include_bytes!` in `src/ui.rs` (`load_font`); nothing is staged loose
into the `.ipk`.
