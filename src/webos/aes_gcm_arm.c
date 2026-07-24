/* Hardware-accelerated AES-128-GCM for ARMv8 (AArch32 and AArch64) using the
 * ARM Cryptography Extensions (AES + PMULL) exposed through <arm_neon.h>.
 *
 * webOS 5.x+ LG SoCs are ARMv8-A cores running a 32-bit (AArch32) userland, so
 * RustCrypto's aes/ghash crates — whose hardware backends are gated to aarch64 —
 * fall back to software here. This module gives punktfunk-core a hardware AES-GCM
 * backend on that target. It compiles identically on aarch64 (used only to
 * validate correctness against NIST vectors on a dev box), since the crypto
 * intrinsics share names across both A-profiles.
 *
 * Wire-compatible with RustCrypto's aes-gcm 0.10 (the host's cipher): 96-bit
 * nonce, 128-bit tag, standard SP800-38D GHASH.
 */
#include <arm_neon.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

/* ---------------------------------------------------------------- AES-128 --- */

static const uint8_t SBOX[256] = {
    0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,
    0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,
    0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,
    0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,
    0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,
    0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,
    0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,
    0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,
    0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,
    0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,
    0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,
    0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,
    0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,
    0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,
    0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,
    0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16,
};

typedef struct { uint8x16_t rk[11]; } aes128_ks;

static void aes128_key_expand(const uint8_t key[16], aes128_ks *ks) {
    uint8_t w[176];
    memcpy(w, key, 16);
    uint8_t rcon = 1;
    for (size_t i = 16; i < 176; i += 4) {
        uint8_t t[4];
        memcpy(t, w + i - 4, 4);
        if (i % 16 == 0) {
            uint8_t tmp = t[0]; t[0] = t[1]; t[1] = t[2]; t[2] = t[3]; t[3] = tmp; /* RotWord */
            for (int j = 0; j < 4; j++) t[j] = SBOX[t[j]];                         /* SubWord */
            t[0] ^= rcon;
            rcon = (uint8_t)((rcon << 1) ^ ((rcon & 0x80) ? 0x1b : 0));            /* xtime  */
        }
        for (int j = 0; j < 4; j++) w[i + j] = w[i - 16 + j] ^ t[j];
    }
    for (int r = 0; r < 11; r++) ks->rk[r] = vld1q_u8(w + r * 16);
}

static inline uint8x16_t aes128_encrypt_block(const aes128_ks *ks, uint8x16_t s) {
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[0]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[1]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[2]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[3]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[4]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[5]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[6]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[7]));
    s = vaesmcq_u8(vaeseq_u8(s, ks->rk[8]));
    s = vaeseq_u8(s, ks->rk[9]);          /* last round: AESE (AddRoundKey+SubBytes+ShiftRows) */
    s = veorq_u8(s, ks->rk[10]);          /* final AddRoundKey, no MixColumns */
    return s;
}

/* ------------------------------------------------------------------ GHASH --- */

/* Reverse the bits within each byte, keeping byte order. GHASH numbers
 * coefficients MSB-first within each byte (bit 7 of byte 0 is x^0); this maps a
 * block to a plain little-endian polynomial (bit k = x^k) so PMULL's natural bit
 * order and the standard x^128 + x^7 + x^2 + x + 1 reduction (constant 0x87)
 * apply directly. Self-inverse, so the same call converts results back.
 *
 * Done with a SWAR shift/mask sequence rather than the VRBIT intrinsic
 * (`vrbitq_u8`), which the webOS AArch32 GCC's <arm_neon.h> does not expose. */
static inline uint8x16_t rev128(uint8x16_t v) {
    const uint8x16_t m1 = vdupq_n_u8(0x55), m2 = vdupq_n_u8(0x33), m4 = vdupq_n_u8(0x0f);
    v = vorrq_u8(vandq_u8(vshrq_n_u8(v, 1), m1), vshlq_n_u8(vandq_u8(v, m1), 1)); /* swap bits */
    v = vorrq_u8(vandq_u8(vshrq_n_u8(v, 2), m2), vshlq_n_u8(vandq_u8(v, m2), 2)); /* swap pairs */
    v = vorrq_u8(vandq_u8(vshrq_n_u8(v, 4), m4), vshlq_n_u8(vandq_u8(v, m4), 4)); /* swap nibbles */
    return v;
}

static inline uint64_t lane0(uint8x16_t v) { return vgetq_lane_u64(vreinterpretq_u64_u8(v), 0); }
static inline uint64_t lane1(uint8x16_t v) { return vgetq_lane_u64(vreinterpretq_u64_u8(v), 1); }

static inline uint8x16_t clmul(uint64_t a, uint64_t b) {
    return vreinterpretq_u8_p128(vmull_p64((poly64_t)a, (poly64_t)b));
}

/* Carryless multiply of a 64-bit value by g = 0x87 (= x^7+x^2+x+1), split into
 * the low 64 result bits and the (<=7) overflow bits above bit 63. */
static inline void mul_by_g(uint64_t a, uint64_t *lo, uint64_t *hi) {
    *lo = a ^ (a << 1) ^ (a << 2) ^ (a << 7);
    *hi = (a >> 63) ^ (a >> 62) ^ (a >> 57);
}

/* GHASH multiply in GF(2^128): returns (Xi * Hr). `Hr` is the hash subkey
 * pre-reversed with rev128() once by the caller; `Xi` is in GHASH byte order. */
static uint8x16_t ghash_mul(uint8x16_t Xi, uint8x16_t Hr) {
    uint8x16_t a = rev128(Xi);
    uint64_t a0 = lane0(a), a1 = lane1(a);
    uint64_t b0 = lane0(Hr), b1 = lane1(Hr);

    uint8x16_t z0 = clmul(a0, b0);
    uint8x16_t z2 = clmul(a1, b1);
    uint8x16_t zm = clmul(a0 ^ a1, b0 ^ b1);

    uint64_t z0l = lane0(z0), z0h = lane1(z0);
    uint64_t z2l = lane0(z2), z2h = lane1(z2);
    uint64_t z1l = lane0(zm) ^ z0l ^ z2l;
    uint64_t z1h = lane1(zm) ^ z0h ^ z2h;

    /* 256-bit product as words w0..w3 (little-endian bit order). */
    uint64_t w0 = z0l;
    uint64_t w1 = z0h ^ z1l;
    uint64_t w2 = z2l ^ z1h;
    uint64_t w3 = z2h;

    /* Reduce mod x^128 + x^7 + x^2 + x + 1: fold the high 128 bits (w2,w3) down. */
    uint64_t l2, h2, l3, h3;
    mul_by_g(w2, &l2, &h2);
    mul_by_g(w3, &l3, &h3);
    w0 ^= l2;
    w1 ^= h2 ^ l3;
    uint64_t of = h3;              /* overflow into bit-128 region (<=7 bits) */
    uint64_t lo, hi;
    mul_by_g(of, &lo, &hi);       /* hi is 0 here */
    w0 ^= lo;

    uint64x2_t r = { w0, w1 };
    return rev128(vreinterpretq_u8_u64(r));
}

/* --------------------------------------------------------------- GCM core --- */

static inline void inc32(uint8_t ctr[16]) {
    for (int i = 15; i >= 12; i--) { if (++ctr[i]) break; }
}

/* Accumulate one 16-byte block (zero-padded by the caller) into the GHASH state. */
static inline uint8x16_t ghash_step(uint8x16_t y, const uint8_t block[16], uint8x16_t Hr) {
    return ghash_mul(veorq_u8(y, vld1q_u8(block)), Hr);
}

static uint8x16_t ghash_bytes(uint8x16_t y, const uint8_t *data, size_t len, uint8x16_t Hr) {
    while (len >= 16) {
        y = ghash_step(y, data, Hr);
        data += 16;
        len -= 16;
    }
    if (len) {
        uint8_t block[16] = {0};
        memcpy(block, data, len);
        y = ghash_step(y, block, Hr);
    }
    return y;
}

/* Compute the auth tag over aad || ciphertext and XOR in E(J0). */
static uint8x16_t gcm_tag(const aes128_ks *ks, uint8x16_t Hr, uint8x16_t ej0,
                          const uint8_t *aad, size_t aad_len,
                          const uint8_t *ct, size_t ct_len) {
    uint8x16_t y = vdupq_n_u8(0);
    y = ghash_bytes(y, aad, aad_len, Hr);
    y = ghash_bytes(y, ct, ct_len, Hr);
    uint8_t lenblk[16];
    uint64_t aad_bits = (uint64_t)aad_len * 8;
    uint64_t ct_bits = (uint64_t)ct_len * 8;
    for (int i = 0; i < 8; i++) lenblk[i] = (uint8_t)(aad_bits >> (56 - 8 * i));
    for (int i = 0; i < 8; i++) lenblk[8 + i] = (uint8_t)(ct_bits >> (56 - 8 * i));
    y = ghash_step(y, lenblk, Hr);
    (void)ks;
    return veorq_u8(y, ej0);
}

/* CTR-mode XOR of `buf` in place, counter starting at J0 (pre-incremented per block). */
static void gcm_ctr(const aes128_ks *ks, uint8_t ctr[16], uint8_t *buf, size_t len) {
    while (len) {
        inc32(ctr);
        uint8x16_t ks_block = aes128_encrypt_block(ks, vld1q_u8(ctr));
        if (len >= 16) {
            vst1q_u8(buf, veorq_u8(vld1q_u8(buf), ks_block));
            buf += 16;
            len -= 16;
        } else {
            uint8_t tmp[16];
            vst1q_u8(tmp, ks_block);
            for (size_t i = 0; i < len; i++) buf[i] ^= tmp[i];
            len = 0;
        }
    }
}

static void gcm_setup(const uint8_t key[16], const uint8_t nonce[12],
                      aes128_ks *ks, uint8x16_t *Hr, uint8x16_t *ej0, uint8_t j0[16]) {
    aes128_key_expand(key, ks);
    uint8x16_t H = aes128_encrypt_block(ks, vdupq_n_u8(0));
    *Hr = rev128(H);
    memcpy(j0, nonce, 12);
    j0[12] = 0; j0[13] = 0; j0[14] = 0; j0[15] = 1;   /* 96-bit IV: J0 = IV || 0^31 1 */
    *ej0 = aes128_encrypt_block(ks, vld1q_u8(j0));
}

static int ct_eq(const uint8_t *a, const uint8_t *b, size_t n) {
    uint8_t d = 0;
    for (size_t i = 0; i < n; i++) d |= a[i] ^ b[i];
    return d == 0;
}

/* ------------------------------------------------------------ public FFI --- */

void pf_aes128_gcm_seal(const uint8_t key[16], const uint8_t nonce[12],
                        const uint8_t *aad, size_t aad_len,
                        uint8_t *buf, size_t len, uint8_t tag[16]) {
    aes128_ks ks;
    uint8x16_t Hr, ej0;
    uint8_t ctr[16];
    gcm_setup(key, nonce, &ks, &Hr, &ej0, ctr);
    gcm_ctr(&ks, ctr, buf, len);                              /* plaintext -> ciphertext */
    vst1q_u8(tag, gcm_tag(&ks, Hr, ej0, aad, aad_len, buf, len));
}

/* Returns 0 on success, -1 on authentication failure (buf left untouched on failure). */
int pf_aes128_gcm_open(const uint8_t key[16], const uint8_t nonce[12],
                       const uint8_t *aad, size_t aad_len,
                       uint8_t *buf, size_t len, const uint8_t tag[16]) {
    aes128_ks ks;
    uint8x16_t Hr, ej0;
    uint8_t ctr[16];
    gcm_setup(key, nonce, &ks, &Hr, &ej0, ctr);
    uint8_t expected[16];
    vst1q_u8(expected, gcm_tag(&ks, Hr, ej0, aad, aad_len, buf, len));  /* tag over ciphertext */
    if (!ct_eq(expected, tag, 16)) return -1;
    gcm_ctr(&ks, ctr, buf, len);                             /* ciphertext -> plaintext */
    return 0;
}
