fn main() {
    // webOS's shipped glibc predates getauxval/gettid/sendmmsg (see
    // glibc_compat_shim.c) — only the real webOS cross target needs the shim; a
    // native Linux dev box's system glibc already has all three.
    if std::env::var("TARGET").as_deref() != Ok("armv7-unknown-linux-gnueabi") {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let cc = std::env::var("CC_armv7_unknown_linux_gnueabi")
        .or_else(|_| std::env::var("CC"))
        .unwrap_or_else(|_| "cc".into());
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

    // The CX's on-device libSDL2 is 2.0.10 (confirmed live: missing SDL_Metal_DestroyView,
    // an ABI symbol our sdl2-sys build expects) — far older than the NDK sysroot's 2.24.1
    // this binary links against. Every other native webOS SDL2 app (aurora-tv/moonlight-tv,
    // RetroArch-webOS) bundles its own newer libSDL2 next to the binary rather than trusting
    // the system's; `task package` (taskfiles/toolchain.yml) copies the exact .so this
    // binary links against into the ipk's lib/ dir (sibling of bin/, where appinfo.json's
    // "main" points), so $ORIGIN is relative to bin/.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib");
}
