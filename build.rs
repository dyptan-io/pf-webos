fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");

    build_spinner_frames(&out_dir, &manifest_dir);

    // webOS's shipped glibc predates getauxval/gettid/sendmmsg (see
    // glibc_compat_shim.c) — only the real webOS cross target needs the shim; a
    // native Linux dev box's system glibc already has all three.
    if std::env::var("TARGET").as_deref() != Ok("armv7-unknown-linux-gnueabi") {
        return;
    }
    let cc = std::env::var("CC_armv7_unknown_linux_gnueabi")
        .or_else(|_| std::env::var("CC"))
        .unwrap_or_else(|_| "cc".into());
    let cxx = std::env::var("CXX_armv7_unknown_linux_gnueabi")
        .or_else(|_| std::env::var("CXX"))
        .unwrap_or_else(|_| "c++".into());

    // ── glibc_compat_shim.c ──────────────────────────────────────────────────
    let obj = format!("{out_dir}/glibc_compat_shim.o");
    let status = std::process::Command::new(&cc)
        // -fPIC: the final binary links -pie (position-independent executable).
        .args(["-fPIC", "-c", "src/glibc_compat_shim.c", "-o"])
        .arg(&obj)
        .status()
        .unwrap_or_else(|e| panic!("run {cc} to compile glibc_compat_shim.c: {e}"));
    assert!(status.success(), "{cc} failed compiling glibc_compat_shim.c");

    // A bare object via `rustc-link-arg` lands at the END of the link line (after
    // every rlib, including libstd) — required here: libstd's undefined references
    // (getauxval, gettid, sendmmsg) must appear BEFORE this object on a single
    // left-to-right linker pass for it to pull the symbols in.
    // `cargo:rustc-link-lib=static=...` (the cc crate's default) places its -l flag
    // right after the crate's own objects instead — too early, so the linker treats
    // it as unneeded and drops it, and the real link still fails undefined.
    println!("cargo:rustc-link-arg={obj}");
    println!("cargo:rerun-if-changed=src/glibc_compat_shim.c");

    // ── starfish_c_shim.cpp → libplayerAPIs_C.so ────────────────────────────
    // `libplayerAPIs.so` on the TV exposes a C++ ABI only; `starfish.rs` expects
    // C-compatible symbols via `dlopen("libplayerAPIs_C.so")`.  We build that
    // wrapper here and the packaging step bundles it in the IPK's lib/ directory.
    //
    // OUT_DIR is structured as target/<target>/<profile>/build/<crate>-<hash>/out;
    // going up 3 levels gives target/<target>/<profile>/ — the same directory
    // the binary lands in, so the Taskfile's `cp` step finds the .so predictably.
    let sysroot = format!(
        "{manifest_dir}/.toolchains/arm-webos-linux-gnueabi_sdk-buildroot\
         /arm-webos-linux-gnueabi/sysroot"
    );
    let include_dir = format!("{sysroot}/usr/include/starfish-media-pipeline");
    let shim_src = format!("{manifest_dir}/src/starfish_c_shim.cpp");
    let release_dir = std::path::PathBuf::from(&out_dir)
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should be 3 ancestor levels above target/<target>/<profile>")
        .to_path_buf();
    let so_out = release_dir.join("libplayerAPIs_C.so");

    let status = std::process::Command::new(&cxx)
        .args(["-shared", "-fPIC", "-std=c++14", "-I", &include_dir])
        .arg(&shim_src)
        .arg("-o")
        .arg(&so_out)
        .arg(format!("-L{sysroot}/usr/lib"))
        .arg("-lplayerAPIs")
        .status()
        .unwrap_or_else(|e| panic!("run {cxx} to compile starfish_c_shim.cpp: {e}"));
    assert!(status.success(), "{cxx} failed compiling starfish_c_shim.cpp");
    println!("cargo:rerun-if-changed=src/starfish_c_shim.cpp");

    // The CX's on-device libSDL2 is 2.0.10 (confirmed live: missing SDL_Metal_DestroyView,
    // an ABI symbol our sdl2-sys build expects) — far older than the NDK sysroot's 2.24.1
    // this binary links against. Every other native webOS SDL2 app (aurora-tv/moonlight-tv,
    // RetroArch-webOS) bundles its own newer libSDL2 next to the binary rather than trusting
    // the system's; `task package` (taskfiles/toolchain.yml) copies the exact .so this
    // binary links against into the ipk's lib/ dir (sibling of bin/, where appinfo.json's
    // "main" points), so $ORIGIN is relative to bin/.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
}

/// Decodes `assets/logo/punktfunk-spinner.gif` into `$OUT_DIR/spinner_frames.bin`
/// for `ui/tiles.rs`'s `spinner_frames` to `include_bytes!` — no GIF/LZW decode
/// on-device. Layout (little-endian): `u32` width, `u32` height, `u32` frame
/// count, then per frame a `u32` delay in milliseconds followed by
/// `width * height * 4` bytes of premultiplied RGBA8, at the GIF's native size
/// (no resize — every frame must share the first frame's dimensions).
fn build_spinner_frames(out_dir: &str, manifest_dir: &str) {
    use image::{codecs::gif::GifDecoder, AnimationDecoder};

    let gif_path = format!("{manifest_dir}/assets/logo/punktfunk-spinner.gif");
    println!("cargo:rerun-if-changed={gif_path}");
    let gif_bytes = std::fs::read(&gif_path).unwrap_or_else(|e| panic!("read {gif_path}: {e}"));
    let decoder = GifDecoder::new(std::io::Cursor::new(gif_bytes.as_slice()))
        .unwrap_or_else(|e| panic!("decode {gif_path}: {e}"));
    let frames = decoder
        .into_frames()
        .collect::<image::ImageResult<Vec<_>>>()
        .unwrap_or_else(|e| panic!("decode {gif_path} frames: {e}"));
    let (width, height) = frames.first().map_or((0, 0), |f| f.buffer().dimensions());

    let mut blob = Vec::new();
    blob.extend_from_slice(&width.to_le_bytes());
    blob.extend_from_slice(&height.to_le_bytes());
    blob.extend_from_slice(&u32::try_from(frames.len()).expect("frame count fits u32").to_le_bytes());
    for frame in frames {
        assert_eq!(
            frame.buffer().dimensions(),
            (width, height),
            "{gif_path}: mismatched frame size"
        );
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay_ms = numer.checked_div(denom).unwrap_or(0);
        let mut rgba = frame.into_buffer().into_raw();
        premultiply_rgba(&mut rgba);
        blob.extend_from_slice(&delay_ms.to_le_bytes());
        blob.extend_from_slice(&rgba);
    }

    std::fs::write(format!("{out_dir}/spinner_frames.bin"), blob).expect("write spinner_frames.bin");
}

/// Straight-alpha -> premultiplied RGBA8, in place — mirrors
/// `ui::painter::premultiply_rgba` (build.rs can't `use` the main crate).
fn premultiply_rgba(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        px[0] = ((u32::from(px[0]) * a) / 255) as u8;
        px[1] = ((u32::from(px[1]) * a) / 255) as u8;
        px[2] = ((u32::from(px[2]) * a) / 255) as u8;
    }
}
