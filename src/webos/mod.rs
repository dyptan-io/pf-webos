//! webOS-platform-specific native (C/C++) shims and their Rust bindings, kept in one
//! place: `glibc_compat_shim.c` (missing glibc symbols), `starfish_c_shim.cpp` (C ABI
//! wrapper for `libplayerAPIs.so`'s C++-only interface), and `aes_gcm_arm.c` (hardware
//! AES-128-GCM). All three are compiled directly by `build.rs`, which is why only
//! `aes_gcm_arm.c` has a corresponding Rust module here — the other two are linked as
//! bare objects/shared libraries with no Rust-side FFI wrapper of their own
//! (`glibc_compat_shim.c`'s symbols are pulled in by libstd itself; `starfish.rs`
//! `dlopen`s `libplayerAPIs_C.so` directly).

pub mod aes;
