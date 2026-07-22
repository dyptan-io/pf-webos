fn main() {
    // webOS's shipped glibc predates getauxval/gettid/sendmmsg (see
    // glibc_compat_shim.c) — only the real webOS cross target needs the shim; a
    // native Linux dev box's system glibc already has all three.
    if std::env::var("TARGET").as_deref() != Ok("armv7-unknown-linux-gnueabi") {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
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
