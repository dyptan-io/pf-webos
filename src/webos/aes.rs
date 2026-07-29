//! Hardware AES-128-GCM for the LG CX, via `punktfunk-core`'s `crypto::Aes128GcmBackend`
//! extension point. webOS's ARMv8-A SoC runs a 32-bit userland where RustCrypto's
//! `aarch64`-gated hardware AES/GHASH falls back to software; `aes_gcm_arm.c` (built by
//! `build.rs`, `armv7-unknown-linux-gnueabi` only) drives the ARM Crypto Extensions
//! directly. Validated against NIST/McGrew GCM vectors, a GHASH fuzz, on-device
//! disassembly (`aese.8`/`aesmc.8`/`vmull.p64` present), and an A/B CPU benchmark
//! (~12% less total CPU than ChaCha20-Poly1305 on the real TV).

use punktfunk_core::crypto::Aes128GcmBackend;
use punktfunk_core::error::{PunktfunkError, Result};

extern "C" {
    fn pf_aes128_gcm_seal(
        key: *const u8,
        nonce: *const u8,
        aad: *const u8,
        aad_len: usize,
        buf: *mut u8,
        len: usize,
        tag: *mut u8,
    );
    fn pf_aes128_gcm_open(
        key: *const u8,
        nonce: *const u8,
        aad: *const u8,
        aad_len: usize,
        buf: *mut u8,
        len: usize,
        tag: *const u8,
    ) -> i32;
}

pub struct WebosAes128Gcm {
    key: [u8; 16],
}

/// Installs [`WebosAes128Gcm`] as `punktfunk_core::crypto`'s AES-128-GCM provider. Must run
/// before the first `SessionCrypto` (i.e. before `session::connect`).
pub fn register() -> Result<()> {
    punktfunk_core::crypto::install_aes128gcm_provider(|key| Box::new(WebosAes128Gcm { key: *key }))
}

impl Aes128GcmBackend for WebosAes128Gcm {
    fn seal_in_place(&self, nonce: &[u8; 12], aad: &[u8], buffer: &mut [u8]) -> Result<[u8; 16]> {
        let mut tag = [0u8; 16];
        // SAFETY: all lengths passed match the slices; an empty slice's dangling pointer
        // is never dereferenced (len 0).
        unsafe {
            pf_aes128_gcm_seal(
                self.key.as_ptr(),
                nonce.as_ptr(),
                aad.as_ptr(),
                aad.len(),
                buffer.as_mut_ptr(),
                buffer.len(),
                tag.as_mut_ptr(),
            );
        }
        Ok(tag)
    }

    fn open_in_place(
        &self,
        nonce: &[u8; 12],
        aad: &[u8],
        buffer: &mut [u8],
        tag: &[u8; 16],
    ) -> Result<()> {
        // SAFETY: as above; the shim verifies the tag before touching `buffer`, leaving it
        // unchanged on an auth failure.
        let rc = unsafe {
            pf_aes128_gcm_open(
                self.key.as_ptr(),
                nonce.as_ptr(),
                aad.as_ptr(),
                aad.len(),
                buffer.as_mut_ptr(),
                buffer.len(),
                tag.as_ptr(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(PunktfunkError::Crypto)
        }
    }
}
