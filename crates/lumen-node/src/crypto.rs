//! Symmetric crypto primitives for `node:crypto`, hand-rolled and dependency-free (the workspace
//! is zero-dep by identity). This backs the JS glue in `src/js/crypto.js`:
//!
//! - AES-128/192/256 block cipher (FIPS 197), with the ECB / CBC / CTR / GCM modes Node exposes as
//!   `aes-<bits>-<mode>`. GCM includes GHASH, the authentication tag, AAD and variable tag/IV
//!   lengths (NIST SP 800-38D).
//! - The scrypt (RFC 7914) memory-hard core: Salsa20/8, BlockMix and ROMix. The PBKDF2-HMAC-SHA256
//!   wrapping stays in JS (it already has a bit-exact PBKDF2), so this op only does the ROMix pass
//!   over the pre-expanded block — the expensive, memory-hard part that must be native to be fast.
//!
//! Every value here is verified bit-exact against Node v22 (`node -e`) by the probe/tests. The
//! padding, streaming and error semantics live in the JS `Cipheriv`/`Decipheriv` classes; the ops
//! below are pure one-shot transforms over whole messages.

use std::sync::OnceLock;

use lumen_host::{ops, Ctx, OpDecl, Value};

/// The op table registered under the `__crypto` namespace (captured by `preamble.js`).
pub const CRYPTO_OPS: &[OpDecl] = ops![
    "aesEcb" (3) => op_aes_ecb,
    "aesCbc" (4) => op_aes_cbc,
    "aesCtr" (3) => op_aes_ctr,
    "aesGcmEncrypt" (5) => op_aes_gcm_encrypt,
    "aesGcmDecrypt" (5) => op_aes_gcm_decrypt,
    "gcmInit" (2) => op_gcm_init,
    "gcmTag" (4) => op_gcm_tag,
    "scryptRomix" (3) => op_scrypt_romix,
    "sha512" (2) => op_sha512,
];

// ---- AES core (FIPS 197) ----------------------------------------------------------------------

/// Multiply two bytes in GF(2^8) with the AES reduction polynomial 0x11B.
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// The AES S-box and its inverse, derived from the GF(2^8) multiplicative inverse plus the affine
/// transform — computed once rather than shipped as a magic 512-byte table.
fn sboxes() -> &'static ([u8; 256], [u8; 256]) {
    static BOXES: OnceLock<([u8; 256], [u8; 256])> = OnceLock::new();
    BOXES.get_or_init(|| {
        // exp[i] = 3^i in GF(2^8); log is its inverse. Together they give the multiplicative
        // inverse cheaply: inv(a) = exp[255 - log[a]].
        let mut exp = [0u8; 256];
        let mut log = [0u8; 256];
        let mut x = 1u8;
        for e in exp.iter_mut().take(255) {
            *e = x;
            x = gmul(x, 3);
        }
        for (i, &v) in exp.iter().take(255).enumerate() {
            log[v as usize] = i as u8;
        }
        let inv = |a: u8| -> u8 {
            if a == 0 {
                0
            } else {
                exp[(255 - log[a as usize] as usize) % 255]
            }
        };
        let mut sbox = [0u8; 256];
        let mut inv_sbox = [0u8; 256];
        for i in 0..256usize {
            let b = inv(i as u8);
            let s =
                b ^ b.rotate_left(1) ^ b.rotate_left(2) ^ b.rotate_left(3) ^ b.rotate_left(4) ^ 0x63;
            sbox[i] = s;
            inv_sbox[s as usize] = i as u8;
        }
        (sbox, inv_sbox)
    })
}

/// An expanded AES key schedule plus round count.
struct Aes {
    /// Round-key words, `4 * (rounds + 1)` of them.
    w: Vec<u32>,
    rounds: usize,
}

impl Aes {
    fn new(key: &[u8]) -> Option<Aes> {
        let nk = match key.len() {
            16 => 4,
            24 => 6,
            32 => 8,
            _ => return None,
        };
        let rounds = nk + 6;
        let total = 4 * (rounds + 1);
        let (sbox, _) = sboxes();
        let mut w = vec![0u32; total];
        for i in 0..nk {
            w[i] = u32::from_be_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
        }
        let sub_word = |word: u32| -> u32 {
            let b = word.to_be_bytes();
            u32::from_be_bytes([
                sbox[b[0] as usize],
                sbox[b[1] as usize],
                sbox[b[2] as usize],
                sbox[b[3] as usize],
            ])
        };
        let mut rcon = 1u8;
        for i in nk..total {
            let mut temp = w[i - 1];
            if i % nk == 0 {
                temp = sub_word(temp.rotate_left(8)) ^ ((rcon as u32) << 24);
                // Advance rcon = xtime(rcon).
                let hi = rcon & 0x80;
                rcon <<= 1;
                if hi != 0 {
                    rcon ^= 0x1b;
                }
            } else if nk > 6 && i % nk == 4 {
                temp = sub_word(temp);
            }
            w[i] = w[i - nk] ^ temp;
        }
        Some(Aes { w, rounds })
    }

    fn add_round_key(state: &mut [u8; 16], w: &[u32]) {
        for c in 0..4 {
            let b = w[c].to_be_bytes();
            state[4 * c] ^= b[0];
            state[4 * c + 1] ^= b[1];
            state[4 * c + 2] ^= b[2];
            state[4 * c + 3] ^= b[3];
        }
    }

    fn encrypt_block(&self, block: &mut [u8; 16]) {
        let (sbox, _) = sboxes();
        Aes::add_round_key(block, &self.w[0..4]);
        for round in 1..self.rounds {
            for byte in block.iter_mut() {
                *byte = sbox[*byte as usize];
            }
            shift_rows(block);
            mix_columns(block);
            Aes::add_round_key(block, &self.w[4 * round..4 * round + 4]);
        }
        for byte in block.iter_mut() {
            *byte = sbox[*byte as usize];
        }
        shift_rows(block);
        Aes::add_round_key(block, &self.w[4 * self.rounds..4 * self.rounds + 4]);
    }

    fn decrypt_block(&self, block: &mut [u8; 16]) {
        let (_, inv_sbox) = sboxes();
        Aes::add_round_key(block, &self.w[4 * self.rounds..4 * self.rounds + 4]);
        for round in (1..self.rounds).rev() {
            inv_shift_rows(block);
            for byte in block.iter_mut() {
                *byte = inv_sbox[*byte as usize];
            }
            Aes::add_round_key(block, &self.w[4 * round..4 * round + 4]);
            inv_mix_columns(block);
        }
        inv_shift_rows(block);
        for byte in block.iter_mut() {
            *byte = inv_sbox[*byte as usize];
        }
        Aes::add_round_key(block, &self.w[0..4]);
    }
}

fn shift_rows(b: &mut [u8; 16]) {
    let mut t = [0u8; 16];
    for r in 0..4 {
        for c in 0..4 {
            t[r + 4 * c] = b[r + 4 * ((c + r) % 4)];
        }
    }
    *b = t;
}

fn inv_shift_rows(b: &mut [u8; 16]) {
    let mut t = [0u8; 16];
    for r in 0..4 {
        for c in 0..4 {
            t[r + 4 * c] = b[r + 4 * ((c + 4 - r) % 4)];
        }
    }
    *b = t;
}

fn mix_columns(b: &mut [u8; 16]) {
    for c in 0..4 {
        let s0 = b[4 * c];
        let s1 = b[4 * c + 1];
        let s2 = b[4 * c + 2];
        let s3 = b[4 * c + 3];
        b[4 * c] = gmul(s0, 2) ^ gmul(s1, 3) ^ s2 ^ s3;
        b[4 * c + 1] = s0 ^ gmul(s1, 2) ^ gmul(s2, 3) ^ s3;
        b[4 * c + 2] = s0 ^ s1 ^ gmul(s2, 2) ^ gmul(s3, 3);
        b[4 * c + 3] = gmul(s0, 3) ^ s1 ^ s2 ^ gmul(s3, 2);
    }
}

fn inv_mix_columns(b: &mut [u8; 16]) {
    for c in 0..4 {
        let s0 = b[4 * c];
        let s1 = b[4 * c + 1];
        let s2 = b[4 * c + 2];
        let s3 = b[4 * c + 3];
        b[4 * c] = gmul(s0, 14) ^ gmul(s1, 11) ^ gmul(s2, 13) ^ gmul(s3, 9);
        b[4 * c + 1] = gmul(s0, 9) ^ gmul(s1, 14) ^ gmul(s2, 11) ^ gmul(s3, 13);
        b[4 * c + 2] = gmul(s0, 13) ^ gmul(s1, 9) ^ gmul(s2, 14) ^ gmul(s3, 11);
        b[4 * c + 3] = gmul(s0, 11) ^ gmul(s1, 13) ^ gmul(s2, 9) ^ gmul(s3, 14);
    }
}

// ---- modes ------------------------------------------------------------------------------------

fn ecb(aes: &Aes, data: &[u8], encrypt: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        if encrypt {
            aes.encrypt_block(&mut block);
        } else {
            aes.decrypt_block(&mut block);
        }
        out.extend_from_slice(&block);
    }
    out
}

fn cbc(aes: &Aes, iv: &[u8; 16], data: &[u8], encrypt: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut prev = *iv;
    for chunk in data.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        if encrypt {
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            aes.encrypt_block(&mut block);
            prev = block;
            out.extend_from_slice(&block);
        } else {
            let cipher = block;
            aes.decrypt_block(&mut block);
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            prev = cipher;
            out.extend_from_slice(&block);
        }
    }
    out
}

/// Increment the whole 128-bit counter block big-endian (Node's CTR mode wraps the full width).
fn inc128(counter: &mut [u8; 16]) {
    for i in (0..16).rev() {
        counter[i] = counter[i].wrapping_add(1);
        if counter[i] != 0 {
            break;
        }
    }
}

fn ctr(aes: &Aes, iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut counter = *iv;
    for chunk in data.chunks(16) {
        let mut ks = counter;
        aes.encrypt_block(&mut ks);
        for (i, &byte) in chunk.iter().enumerate() {
            out.push(byte ^ ks[i]);
        }
        inc128(&mut counter);
    }
    out
}

// ---- GCM (NIST SP 800-38D) --------------------------------------------------------------------

/// GF(2^128) multiply used by GHASH (bit-reflected, reduction polynomial 0xE1<<120).
fn gf_mult(x: &[u8; 16], y: &[u8; 16]) -> [u8; 16] {
    let mut z = [0u8; 16];
    let mut v = *y;
    for i in 0..128 {
        let bit = (x[i / 8] >> (7 - (i % 8))) & 1;
        if bit == 1 {
            for j in 0..16 {
                z[j] ^= v[j];
            }
        }
        let lsb = v[15] & 1;
        let mut carry = 0u8;
        for byte in v.iter_mut() {
            let new_carry = *byte & 1;
            *byte = (*byte >> 1) | (carry << 7);
            carry = new_carry;
        }
        if lsb == 1 {
            v[0] ^= 0xe1;
        }
    }
    z
}

/// GHASH over already-padded, block-aligned data.
fn ghash(h: &[u8; 16], data: &[u8]) -> [u8; 16] {
    let mut y = [0u8; 16];
    for chunk in data.chunks(16) {
        for (i, &b) in chunk.iter().enumerate() {
            y[i] ^= b;
        }
        y = gf_mult(&y, h);
    }
    y
}

/// Increment only the rightmost 32 bits (GCM's inc32).
fn inc32(block: &mut [u8; 16]) {
    for i in (12..16).rev() {
        block[i] = block[i].wrapping_add(1);
        if block[i] != 0 {
            break;
        }
    }
}

/// GCTR: CTR-style keystream starting from `icb`, incrementing the low 32 bits.
fn gctr(aes: &Aes, icb: &[u8; 16], data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut counter = *icb;
    for chunk in data.chunks(16) {
        let mut ks = counter;
        aes.encrypt_block(&mut ks);
        for (i, &b) in chunk.iter().enumerate() {
            out.push(b ^ ks[i]);
        }
        inc32(&mut counter);
    }
    out
}

/// Compute J0 (the pre-counter block) from the IV, per SP 800-38D §7.1.
fn gcm_j0(h: &[u8; 16], iv: &[u8]) -> [u8; 16] {
    if iv.len() == 12 {
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
        j0
    } else {
        // GHASH(IV padded to a block multiple || 0^64 || len(IV)-in-bits as 64-bit BE).
        let mut buf = Vec::new();
        buf.extend_from_slice(iv);
        while buf.len() % 16 != 0 {
            buf.push(0);
        }
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&((iv.len() as u64) * 8).to_be_bytes());
        ghash(h, &buf)
    }
}

/// The GHASH input for the tag: AAD (zero-padded) || C (zero-padded) || len(A)||len(C) in bits.
fn gcm_tag_input(aad: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut s = Vec::new();
    s.extend_from_slice(aad);
    while s.len() % 16 != 0 {
        s.push(0);
    }
    s.extend_from_slice(ciphertext);
    while s.len() % 16 != 0 {
        s.push(0);
    }
    s.extend_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
    s.extend_from_slice(&((ciphertext.len() as u64) * 8).to_be_bytes());
    s
}

/// Returns `(ciphertext, full 16-byte tag)`.
fn gcm_encrypt(aes: &Aes, iv: &[u8], aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, [u8; 16]) {
    let mut h = [0u8; 16];
    aes.encrypt_block(&mut h);
    let j0 = gcm_j0(&h, iv);
    let mut icb = j0;
    inc32(&mut icb);
    let ciphertext = gctr(aes, &icb, plaintext);
    let s = ghash(&h, &gcm_tag_input(aad, &ciphertext));
    let full_tag = gctr(aes, &j0, &s);
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&full_tag);
    (ciphertext, tag)
}

/// Returns `Some(plaintext)` when the tag verifies, `None` otherwise.
fn gcm_decrypt(aes: &Aes, iv: &[u8], aad: &[u8], ciphertext: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
    let mut h = [0u8; 16];
    aes.encrypt_block(&mut h);
    let j0 = gcm_j0(&h, iv);
    let s = ghash(&h, &gcm_tag_input(aad, ciphertext));
    let full_tag = gctr(aes, &j0, &s);
    // Constant-time compare over the provided (possibly truncated) tag length.
    let mut diff = 0u8;
    for (i, &t) in tag.iter().enumerate() {
        diff |= full_tag[i] ^ t;
    }
    if diff != 0 {
        return None;
    }
    let mut icb = j0;
    inc32(&mut icb);
    Some(gctr(aes, &icb, ciphertext))
}

// ---- scrypt core (RFC 7914) -------------------------------------------------------------------

fn salsa20_8(block: &mut [u8; 64]) {
    let mut x = [0u32; 16];
    for (i, word) in x.iter_mut().enumerate() {
        *word = u32::from_le_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    let orig = x;
    let rotl = |v: u32, n: u32| v.rotate_left(n);
    for _ in 0..4 {
        // Column round.
        x[4] ^= rotl(x[0].wrapping_add(x[12]), 7);
        x[8] ^= rotl(x[4].wrapping_add(x[0]), 9);
        x[12] ^= rotl(x[8].wrapping_add(x[4]), 13);
        x[0] ^= rotl(x[12].wrapping_add(x[8]), 18);
        x[9] ^= rotl(x[5].wrapping_add(x[1]), 7);
        x[13] ^= rotl(x[9].wrapping_add(x[5]), 9);
        x[1] ^= rotl(x[13].wrapping_add(x[9]), 13);
        x[5] ^= rotl(x[1].wrapping_add(x[13]), 18);
        x[14] ^= rotl(x[10].wrapping_add(x[6]), 7);
        x[2] ^= rotl(x[14].wrapping_add(x[10]), 9);
        x[6] ^= rotl(x[2].wrapping_add(x[14]), 13);
        x[10] ^= rotl(x[6].wrapping_add(x[2]), 18);
        x[3] ^= rotl(x[15].wrapping_add(x[11]), 7);
        x[7] ^= rotl(x[3].wrapping_add(x[15]), 9);
        x[11] ^= rotl(x[7].wrapping_add(x[3]), 13);
        x[15] ^= rotl(x[11].wrapping_add(x[7]), 18);
        // Row round.
        x[1] ^= rotl(x[0].wrapping_add(x[3]), 7);
        x[2] ^= rotl(x[1].wrapping_add(x[0]), 9);
        x[3] ^= rotl(x[2].wrapping_add(x[1]), 13);
        x[0] ^= rotl(x[3].wrapping_add(x[2]), 18);
        x[6] ^= rotl(x[5].wrapping_add(x[4]), 7);
        x[7] ^= rotl(x[6].wrapping_add(x[5]), 9);
        x[4] ^= rotl(x[7].wrapping_add(x[6]), 13);
        x[5] ^= rotl(x[4].wrapping_add(x[7]), 18);
        x[11] ^= rotl(x[10].wrapping_add(x[9]), 7);
        x[8] ^= rotl(x[11].wrapping_add(x[10]), 9);
        x[9] ^= rotl(x[8].wrapping_add(x[11]), 13);
        x[10] ^= rotl(x[9].wrapping_add(x[8]), 18);
        x[12] ^= rotl(x[15].wrapping_add(x[14]), 7);
        x[13] ^= rotl(x[12].wrapping_add(x[15]), 9);
        x[14] ^= rotl(x[13].wrapping_add(x[12]), 13);
        x[15] ^= rotl(x[14].wrapping_add(x[13]), 18);
    }
    for i in 0..16 {
        let v = x[i].wrapping_add(orig[i]);
        block[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// BlockMix over `2r` 64-byte blocks (RFC 7914 §7), returning the reordered output.
fn block_mix(b: &[u8], r: usize) -> Vec<u8> {
    let two_r = 2 * r;
    let mut x = [0u8; 64];
    x.copy_from_slice(&b[(two_r - 1) * 64..two_r * 64]);
    let mut y = vec![0u8; two_r * 64];
    for i in 0..two_r {
        for j in 0..64 {
            x[j] ^= b[i * 64 + j];
        }
        salsa20_8(&mut x);
        // Even i -> first half, odd i -> second half (the RFC's interleave).
        let dest = if i % 2 == 0 {
            (i / 2) * 64
        } else {
            (r + i / 2) * 64
        };
        y[dest..dest + 64].copy_from_slice(&x);
    }
    y
}

fn integerify(x: &[u8], r: usize) -> u64 {
    let off = (2 * r - 1) * 64;
    u64::from_le_bytes([
        x[off],
        x[off + 1],
        x[off + 2],
        x[off + 3],
        x[off + 4],
        x[off + 5],
        x[off + 6],
        x[off + 7],
    ])
}

fn romix(block: &mut [u8], n: u64, r: usize) {
    let block_len = 128 * r;
    let mut v = vec![0u8; block_len * n as usize];
    let mut x = block.to_vec();
    for i in 0..n as usize {
        v[i * block_len..(i + 1) * block_len].copy_from_slice(&x);
        x = block_mix(&x, r);
    }
    for _ in 0..n {
        let j = (integerify(&x, r) % n) as usize;
        for k in 0..block_len {
            x[k] ^= v[j * block_len + k];
        }
        x = block_mix(&x, r);
    }
    block.copy_from_slice(&x);
}

// ---- SHA-512 family (FIPS 180-4) --------------------------------------------------------------
// One 64-bit core; the variants differ only in the initial hash value and the output truncation.
// SHA-384/512/512-224/512-256 all share the 128-byte block (so HMAC blockSize is 128 in the glue).

#[rustfmt::skip]
const K512: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd, 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019, 0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe, 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1, 0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3, 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483, 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210, 0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725, 0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926, 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8, 0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001, 0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910, 0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53, 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb, 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60, 0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9, 0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207, 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6, 0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493, 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a, 0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

fn sha512_core(data: &[u8], init: &[u64; 8], out_len: usize) -> Vec<u8> {
    let mut h = *init;
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u128) * 8;
    msg.push(0x80);
    while msg.len() % 128 != 112 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_be_bytes());

    let mut w = [0u64; 80];
    for block in msg.chunks_exact(128) {
        for (i, word) in block.chunks_exact(8).enumerate() {
            w[i] = u64::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K512[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (v, add) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *v = v.wrapping_add(add);
        }
    }

    let mut out = Vec::with_capacity(64);
    for word in h {
        out.extend_from_slice(&word.to_be_bytes());
    }
    out.truncate(out_len);
    out
}

/// The initial hash value + output length for each SHA-512 variant.
fn sha512_variant(v: u32) -> (&'static [u64; 8], usize) {
    // 0 = sha512, 1 = sha384, 2 = sha512-224, 3 = sha512-256.
    match v {
        1 => (
            &[
                0xcbbb9d5dc1059ed8, 0x629a292a367cd507, 0x9159015a3070dd17, 0x152fecd8f70e5939,
                0x67332667ffc00b31, 0x8eb44a8768581511, 0xdb0c2e0d64f98fa7, 0x47b5481dbefa4fa4,
            ],
            48,
        ),
        2 => (
            &[
                0x8c3d37c819544da2, 0x73e1996689dcd4d6, 0x1dfab7ae32ff9c82, 0x679dd514582f9fcf,
                0x0f6d2b697bd44da8, 0x77e36f7304c48942, 0x3f9d85a86a1d36c8, 0x1112e6ad91d692a1,
            ],
            28,
        ),
        3 => (
            &[
                0x22312194fc2bf72c, 0x9f555fa3c84c64c2, 0x2393b86b6f53b151, 0x963877195940eabd,
                0x96283ee2a88effe3, 0xbe5e1e2553863992, 0x2b0199fc2c85b8aa, 0x0eb72ddc81c52ca2,
            ],
            32,
        ),
        _ => (
            &[
                0x6a09e667f3bcc908, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
                0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
            ],
            64,
        ),
    }
}

// ---- ops --------------------------------------------------------------------------------------

fn bytes_arg(ctx: &Ctx, args: &[Value], idx: usize) -> Option<Vec<u8>> {
    ctx.typed_array_bytes(args.get(idx)?)
}

fn make_aes(ctx: &mut Ctx, key: &[u8]) -> Result<Aes, Value> {
    Aes::new(key).ok_or_else(|| ctx.make_error("Error", "Invalid AES key length"))
}

fn op_aes_ecb(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let encrypt = matches!(args.first(), Some(Value::Bool(true)));
    let key =
        bytes_arg(ctx, args, 1).ok_or_else(|| ctx.make_error("TypeError", "key must be a buffer"))?;
    let data = bytes_arg(ctx, args, 2)
        .ok_or_else(|| ctx.make_error("TypeError", "data must be a buffer"))?;
    if data.len() % 16 != 0 {
        return Err(ctx.make_error("Error", "aes-ecb: data not block-aligned"));
    }
    let aes = make_aes(ctx, &key)?;
    let out = ecb(&aes, &data, encrypt);
    ctx.make_uint8array(&out)
}

fn op_aes_cbc(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let encrypt = matches!(args.first(), Some(Value::Bool(true)));
    let key =
        bytes_arg(ctx, args, 1).ok_or_else(|| ctx.make_error("TypeError", "key must be a buffer"))?;
    let iv =
        bytes_arg(ctx, args, 2).ok_or_else(|| ctx.make_error("TypeError", "iv must be a buffer"))?;
    let data = bytes_arg(ctx, args, 3)
        .ok_or_else(|| ctx.make_error("TypeError", "data must be a buffer"))?;
    if iv.len() != 16 {
        return Err(ctx.make_error("Error", "aes-cbc: iv must be 16 bytes"));
    }
    if data.len() % 16 != 0 {
        return Err(ctx.make_error("Error", "aes-cbc: data not block-aligned"));
    }
    let aes = make_aes(ctx, &key)?;
    let mut iv16 = [0u8; 16];
    iv16.copy_from_slice(&iv);
    let out = cbc(&aes, &iv16, &data, encrypt);
    ctx.make_uint8array(&out)
}

fn op_aes_ctr(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key =
        bytes_arg(ctx, args, 0).ok_or_else(|| ctx.make_error("TypeError", "key must be a buffer"))?;
    let iv =
        bytes_arg(ctx, args, 1).ok_or_else(|| ctx.make_error("TypeError", "iv must be a buffer"))?;
    let data = bytes_arg(ctx, args, 2)
        .ok_or_else(|| ctx.make_error("TypeError", "data must be a buffer"))?;
    if iv.len() != 16 {
        return Err(ctx.make_error("Error", "aes-ctr: iv must be 16 bytes"));
    }
    let aes = make_aes(ctx, &key)?;
    let mut iv16 = [0u8; 16];
    iv16.copy_from_slice(&iv);
    let out = ctr(&aes, &iv16, &data);
    ctx.make_uint8array(&out)
}

fn op_aes_gcm_encrypt(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key =
        bytes_arg(ctx, args, 0).ok_or_else(|| ctx.make_error("TypeError", "key must be a buffer"))?;
    let iv =
        bytes_arg(ctx, args, 1).ok_or_else(|| ctx.make_error("TypeError", "iv must be a buffer"))?;
    let aad = bytes_arg(ctx, args, 2).unwrap_or_default();
    let data = bytes_arg(ctx, args, 3)
        .ok_or_else(|| ctx.make_error("TypeError", "data must be a buffer"))?;
    let tag_len = args.get(4).and_then(|v| v.as_num_opt()).unwrap_or(16.0) as usize;
    let aes = make_aes(ctx, &key)?;
    let (ciphertext, tag) = gcm_encrypt(&aes, &iv, &aad, &data);
    // Return ciphertext || tag(tagLen); JS slices the tag off the end.
    let mut out = ciphertext;
    out.extend_from_slice(&tag[..tag_len.min(16)]);
    ctx.make_uint8array(&out)
}

fn op_aes_gcm_decrypt(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key =
        bytes_arg(ctx, args, 0).ok_or_else(|| ctx.make_error("TypeError", "key must be a buffer"))?;
    let iv =
        bytes_arg(ctx, args, 1).ok_or_else(|| ctx.make_error("TypeError", "iv must be a buffer"))?;
    let aad = bytes_arg(ctx, args, 2).unwrap_or_default();
    let data = bytes_arg(ctx, args, 3)
        .ok_or_else(|| ctx.make_error("TypeError", "data must be a buffer"))?;
    let tag = bytes_arg(ctx, args, 4)
        .ok_or_else(|| ctx.make_error("TypeError", "tag must be a buffer"))?;
    let aes = make_aes(ctx, &key)?;
    match gcm_decrypt(&aes, &iv, &aad, &data, &tag) {
        // JS distinguishes null (auth failure) from a valid (possibly empty) Uint8Array.
        Some(plaintext) => ctx.make_uint8array(&plaintext),
        None => Ok(Value::Null),
    }
}

/// `gcmInit(key, iv)` — the initial GCM counter block `inc32(J0)`, so the JS glue can stream the
/// GCTR keystream itself (via batched `aesEcb` calls) while Rust keeps the GHASH math.
fn op_gcm_init(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key =
        bytes_arg(ctx, args, 0).ok_or_else(|| ctx.make_error("TypeError", "key must be a buffer"))?;
    let iv =
        bytes_arg(ctx, args, 1).ok_or_else(|| ctx.make_error("TypeError", "iv must be a buffer"))?;
    let aes = make_aes(ctx, &key)?;
    let mut h = [0u8; 16];
    aes.encrypt_block(&mut h);
    let mut j0 = gcm_j0(&h, &iv);
    inc32(&mut j0);
    ctx.make_uint8array(&j0)
}

/// `gcmTag(key, iv, aad, ciphertext)` — the full 16-byte GCM authentication tag over already
/// accumulated ciphertext (both encrypt and decrypt sides verify/emit through this).
fn op_gcm_tag(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let key =
        bytes_arg(ctx, args, 0).ok_or_else(|| ctx.make_error("TypeError", "key must be a buffer"))?;
    let iv =
        bytes_arg(ctx, args, 1).ok_or_else(|| ctx.make_error("TypeError", "iv must be a buffer"))?;
    let aad = bytes_arg(ctx, args, 2).unwrap_or_default();
    let ct = bytes_arg(ctx, args, 3)
        .ok_or_else(|| ctx.make_error("TypeError", "ciphertext must be a buffer"))?;
    let aes = make_aes(ctx, &key)?;
    let mut h = [0u8; 16];
    aes.encrypt_block(&mut h);
    let j0 = gcm_j0(&h, &iv);
    let s = ghash(&h, &gcm_tag_input(&aad, &ct));
    let tag = gctr(&aes, &j0, &s);
    ctx.make_uint8array(&tag)
}

/// `sha512(variant, data)` — one-shot SHA-512-family digest.
/// Variant: 0 = sha512, 1 = sha384, 2 = sha512-224, 3 = sha512-256.
fn op_sha512(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let v = args.first().and_then(|v| v.as_num_opt()).unwrap_or(0.0) as u32;
    let data = bytes_arg(ctx, args, 1)
        .ok_or_else(|| ctx.make_error("TypeError", "data must be a buffer"))?;
    let (init, out_len) = sha512_variant(v);
    let out = sha512_core(&data, init, out_len);
    ctx.make_uint8array(&out)
}

/// `scryptRomix(B, N, r)` — run ROMix over each `128*r`-byte block of `B` (there are `p` of them).
/// `B` comes from the JS PBKDF2 expansion; the result is fed back into the final PBKDF2 pass.
fn op_scrypt_romix(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let mut b = bytes_arg(ctx, args, 0)
        .ok_or_else(|| ctx.make_error("TypeError", "B must be a buffer"))?;
    let n = args.get(1).and_then(|v| v.as_num_opt()).unwrap_or(0.0) as u64;
    let r = args.get(2).and_then(|v| v.as_num_opt()).unwrap_or(0.0) as usize;
    if n < 2 || r == 0 || (n & (n - 1)) != 0 {
        return Err(ctx.make_error("Error", "scrypt: invalid N/r"));
    }
    let block_len = 128 * r;
    if b.is_empty() || b.len() % block_len != 0 {
        return Err(ctx.make_error("Error", "scrypt: B length not a multiple of 128*r"));
    }
    let p = b.len() / block_len;
    for i in 0..p {
        romix(&mut b[i * block_len..(i + 1) * block_len], n, r);
    }
    ctx.make_uint8array(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn aes_fips197_block() {
        // FIPS-197 Appendix B / C.1 known-answer vectors.
        let key = unhex("000102030405060708090a0b0c0d0e0f");
        let aes = Aes::new(&key).unwrap();
        let mut b = [0u8; 16];
        b.copy_from_slice(&unhex("00112233445566778899aabbccddeeff"));
        aes.encrypt_block(&mut b);
        assert_eq!(hex(&b), "69c4e0d86a7b0430d8cdb78070b4c55a");
        aes.decrypt_block(&mut b);
        assert_eq!(hex(&b), "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn aes256_block() {
        let key = unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
        let aes = Aes::new(&key).unwrap();
        let mut b = [0u8; 16];
        b.copy_from_slice(&unhex("00112233445566778899aabbccddeeff"));
        aes.encrypt_block(&mut b);
        assert_eq!(hex(&b), "8ea2b7ca516745bfeafc49904b496089");
    }

    #[test]
    fn gcm_nist_vector() {
        // NIST GCM test case 4 (AES-128, with AAD stripped -> case 3 shape).
        let key = unhex("feffe9928665731c6d6a8f9467308308");
        let iv = unhex("cafebabefacedbaddecaf888");
        let pt = unhex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
        );
        let aes = Aes::new(&key).unwrap();
        let (ct, tag) = gcm_encrypt(&aes, &iv, &[], &pt);
        assert_eq!(hex(&ct), "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091");
        assert_eq!(hex(&tag), "cc15abcc191161501aabab46b8fbac85");
        assert_eq!(
            gcm_decrypt(&aes, &iv, &[], &ct, &tag).as_deref(),
            Some(pt.as_slice())
        );
    }

    #[test]
    fn gcm_tamper_rejected() {
        let key = unhex("feffe9928665731c6d6a8f9467308308");
        let iv = unhex("cafebabefacedbaddecaf888");
        let aes = Aes::new(&key).unwrap();
        let (mut ct, tag) = gcm_encrypt(&aes, &iv, b"aad", b"hello world");
        ct[0] ^= 1;
        assert!(gcm_decrypt(&aes, &iv, b"aad", &ct, &tag).is_none());
    }

    #[test]
    fn ctr_roundtrip() {
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = unhex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let aes = Aes::new(&key).unwrap();
        let mut iv16 = [0u8; 16];
        iv16.copy_from_slice(&iv);
        let pt = unhex("6bc1bee22e409f96e93d7e117393172a");
        let ct = ctr(&aes, &iv16, &pt);
        // NIST SP 800-38A F.5.1 CTR-AES128 first block.
        assert_eq!(hex(&ct), "874d6191b620e3261bef6864990db6ce");
        assert_eq!(ctr(&aes, &iv16, &ct), pt);
    }

    #[test]
    fn sha512_family_fips_vectors() {
        // FIPS 180-4 "abc" known answers for all four variants.
        let (i512, l512) = sha512_variant(0);
        assert_eq!(
            hex(&sha512_core(b"abc", i512, l512)),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        let (i384, l384) = sha512_variant(1);
        assert_eq!(
            hex(&sha512_core(b"abc", i384, l384)),
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
        );
        let (i224, l224) = sha512_variant(2);
        assert_eq!(
            hex(&sha512_core(b"abc", i224, l224)),
            "4634270f707b6a54daae7530460842e20e37ed265ceee9a43e8924aa"
        );
        let (i256, l256) = sha512_variant(3);
        assert_eq!(
            hex(&sha512_core(b"abc", i256, l256)),
            "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23"
        );
        // Multi-block (>128 bytes) sanity.
        assert_eq!(
            hex(&sha512_core(&[b'a'; 200], i512, l512))[..16],
            *"4b11459c33f52a22"
        );
    }

    #[test]
    fn salsa20_8_rfc_vector() {
        // RFC 7914 §8 Salsa20/8 known-answer.
        let mut block = [0u8; 64];
        block.copy_from_slice(&unhex(
            "7e879a214f3ec9867ca940e641718f26baee555b8c61c1b50df846116dcd3b1dee24f319df9b3d8514121e4b5ac5aa3276021d2909c74829edebc68db8b8c25e",
        ));
        salsa20_8(&mut block);
        assert_eq!(
            hex(&block),
            "a41f859c6608cc993b81cacb020cef05044b2181a2fd337dfd7b1c6396682f29b4393168e3c9e6bcfe6bc5b7a06d96bae424cc102c91745c24ad673dc7618f81"
        );
    }
}
