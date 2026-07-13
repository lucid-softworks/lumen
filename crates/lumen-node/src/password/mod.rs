//! `Bun.password` backing: hand-rolled SHA-512 (FIPS 180-4), BLAKE2b (RFC 7693),
//! Argon2 (RFC 9106) and bcrypt (OpenBSD EksBlowfish), plus the PHC / `$2b$` string
//! codecs. Behavior is matched against Bun v1.2.21 (which fronts Zig's std.crypto);
//! see the cross-oracle tests in `tests/password_oracle.rs`.
//!
//! Bun/Zig behaviors replicated deliberately (each verified against the oracle):
//! - argon2 defaults: argon2id, v=19, m=65536 KiB, t=2, p=1, 32-byte salt, 32-byte tag,
//!   standard base64 without padding in the PHC string.
//! - argon2 memory is clamped internally to `max(m, 8*lanes)` but the *requested* m is
//!   what the PHC string records — Bun happily emits (and verifies) `m=1`.
//! - bcrypt emits `$2b$`, cost 4..=31 (default 10), and pre-hashes passwords longer
//!   than 72 bytes with SHA-512 (the raw 64-byte digest becomes the key). The C-string
//!   NUL is appended to the (truncated) key, exactly like OpenBSD.
//! - verify auto-detects the algorithm from the prefix; `$2a$`/`$2x$`/`$2y$` are all
//!   accepted as bcrypt aliases (Bun verifies every minor identically). Errors carry
//!   Bun's exact "Password verification failed with error \"...\"" messages.

use std::io::Read;

use lumen_host::{ops, Ctx, OpDecl, TaskRegistry, Value};

mod constants;

use constants::{BF_P, BF_S, SHA512_H, SHA512_K};

// ---- SHA-512 (FIPS 180-4) ----------------------------------------------------------------------

/// SHA-512 of `data` — bcrypt's >72-byte pre-hash (lumen's crypto module tops out at
/// SHA-256, so this is the only SHA-512 in the tree).
pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut h = SHA512_H;
    let mut msg = Vec::with_capacity(data.len() + 129);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 128 != 112 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u128) * 8).to_be_bytes());
    for block in msg.chunks_exact(128) {
        let mut w = [0u64; 80];
        for (i, c) in block.chunks_exact(8).enumerate() {
            w[i] = u64::from_be_bytes(c.try_into().unwrap());
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
                .wrapping_add(SHA512_K[i])
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
        for (o, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *o = o.wrapping_add(v);
        }
    }
    let mut out = [0u8; 64];
    for (o, v) in out.chunks_exact_mut(8).zip(h) {
        o.copy_from_slice(&v.to_be_bytes());
    }
    out
}

// ---- BLAKE2b (RFC 7693) ------------------------------------------------------------------------

/// BLAKE2b's IV is SHA-512's initial state.
const BLAKE2B_IV: [u64; 8] = SHA512_H;

const BLAKE2B_SIGMA: [[usize; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

/// Incremental unkeyed BLAKE2b with a 1..=64 byte digest (all Argon2 needs).
pub struct Blake2b {
    h: [u64; 8],
    t: u128,
    buf: [u8; 128],
    buf_len: usize,
    out_len: usize,
}

impl Blake2b {
    pub fn new(out_len: usize) -> Blake2b {
        debug_assert!((1..=64).contains(&out_len));
        let mut h = BLAKE2B_IV;
        h[0] ^= 0x0101_0000 ^ out_len as u64;
        Blake2b {
            h,
            t: 0,
            buf: [0; 128],
            buf_len: 0,
            out_len,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) -> &mut Blake2b {
        while !data.is_empty() {
            // A full buffer is compressed only once more input arrives: the final block
            // must go through the `last` path in finalize().
            if self.buf_len == 128 {
                self.t += 128;
                let block = self.buf;
                self.compress(&block, false);
                self.buf_len = 0;
            }
            let n = (128 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + n].copy_from_slice(&data[..n]);
            self.buf_len += n;
            data = &data[n..];
        }
        self
    }

    pub fn finalize(mut self) -> Vec<u8> {
        self.t += self.buf_len as u128;
        for b in &mut self.buf[self.buf_len..] {
            *b = 0;
        }
        let block = self.buf;
        self.compress(&block, true);
        let mut out = vec![0u8; self.out_len];
        for (i, o) in out.iter_mut().enumerate() {
            *o = (self.h[i / 8] >> (8 * (i % 8))) as u8;
        }
        out
    }

    fn compress(&mut self, block: &[u8; 128], last: bool) {
        let mut m = [0u64; 16];
        for (i, c) in block.chunks_exact(8).enumerate() {
            m[i] = u64::from_le_bytes(c.try_into().unwrap());
        }
        let mut v = [0u64; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&BLAKE2B_IV);
        v[12] ^= self.t as u64;
        v[13] ^= (self.t >> 64) as u64;
        if last {
            v[14] = !v[14];
        }
        for s in &BLAKE2B_SIGMA {
            Self::g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
            Self::g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
            Self::g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
            Self::g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
            Self::g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
            Self::g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
            Self::g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
            Self::g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
        }
        for i in 0..8 {
            self.h[i] ^= v[i] ^ v[i + 8];
        }
    }

    fn g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
        v[d] = (v[d] ^ v[a]).rotate_right(32);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(24);
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
        v[d] = (v[d] ^ v[a]).rotate_right(16);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(63);
    }
}

/// One-shot BLAKE2b.
pub fn blake2b(out_len: usize, data: &[u8]) -> Vec<u8> {
    let mut h = Blake2b::new(out_len);
    h.update(data);
    h.finalize()
}

// ---- Argon2 (RFC 9106) -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Argon2Variant {
    Argon2d = 0,
    Argon2i = 1,
    Argon2id = 2,
}

impl Argon2Variant {
    fn phc_name(self) -> &'static str {
        match self {
            Argon2Variant::Argon2d => "argon2d",
            Argon2Variant::Argon2i => "argon2i",
            Argon2Variant::Argon2id => "argon2id",
        }
    }
    pub fn from_phc(s: &str) -> Option<Argon2Variant> {
        match s {
            "argon2d" => Some(Argon2Variant::Argon2d),
            "argon2i" => Some(Argon2Variant::Argon2i),
            "argon2id" => Some(Argon2Variant::Argon2id),
            _ => None,
        }
    }
}

pub struct Argon2Params {
    /// Requested memory in KiB — recorded verbatim in the PHC string; clamped to
    /// `max(m, 8*lanes)` for the actual matrix (Zig's behavior, hence Bun's).
    pub m_cost: u32,
    pub t_cost: u32,
    pub lanes: u32,
    pub variant: Argon2Variant,
    /// 19 (0x13, current) or 16 (0x10, legacy; overwrite instead of XOR on later passes).
    pub version: u32,
    pub out_len: usize,
    /// Optional secret input (the password-hashing "pepper" in RFC 9106 terminology).
    pub secret: Vec<u8>,
    /// Optional public context bound into the derived key.
    pub associated_data: Vec<u8>,
}

/// One 1 KiB memory block, viewed as 128 little-endian u64s.
#[derive(Clone, Copy)]
struct Block([u64; 128]);

impl Block {
    const ZERO: Block = Block([0; 128]);
    fn xor_from(&mut self, other: &Block) {
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a ^= b;
        }
    }
    fn from_bytes(bytes: &[u8]) -> Block {
        debug_assert_eq!(bytes.len(), 1024);
        let mut b = Block::ZERO;
        for (w, c) in b.0.iter_mut().zip(bytes.chunks_exact(8)) {
            *w = u64::from_le_bytes(c.try_into().unwrap());
        }
        b
    }
    fn to_bytes(self) -> [u8; 1024] {
        let mut out = [0u8; 1024];
        for (c, w) in out.chunks_exact_mut(8).zip(self.0) {
            c.copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}

/// The BlaMka mixing primitive: `a + b + 2 * lo32(a) * lo32(b)`.
fn blamka(x: u64, y: u64) -> u64 {
    let lo = (x as u32 as u64).wrapping_mul(y as u32 as u64);
    x.wrapping_add(y).wrapping_add(lo.wrapping_mul(2))
}

fn bg(v: &mut [u64; 128], a: usize, b: usize, c: usize, d: usize) {
    v[a] = blamka(v[a], v[b]);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = blamka(v[c], v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = blamka(v[a], v[b]);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = blamka(v[c], v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// The BLAKE2b-with-multiplication round applied to 16 selected words of a block.
fn permute(v: &mut [u64; 128], i: &[usize; 16]) {
    bg(v, i[0], i[4], i[8], i[12]);
    bg(v, i[1], i[5], i[9], i[13]);
    bg(v, i[2], i[6], i[10], i[14]);
    bg(v, i[3], i[7], i[11], i[15]);
    bg(v, i[0], i[5], i[10], i[15]);
    bg(v, i[1], i[6], i[11], i[12]);
    bg(v, i[2], i[7], i[8], i[13]);
    bg(v, i[3], i[4], i[9], i[14]);
}

/// The compression function G: `next = P(prev ^ ref) ^ prev ^ ref` (xored into the old
/// `next` on 2nd+ passes of v19).
fn fill_block(prev: &Block, reference: &Block, next: &mut Block, with_xor: bool) {
    let mut r = *prev;
    r.xor_from(reference);
    let mut z = r;
    for row in 0..8 {
        let mut idx = [0usize; 16];
        for (k, id) in idx.iter_mut().enumerate() {
            *id = row * 16 + k;
        }
        permute(&mut z.0, &idx);
    }
    for col in 0..8 {
        let mut idx = [0usize; 16];
        for i in 0..8 {
            idx[2 * i] = 16 * i + 2 * col;
            idx[2 * i + 1] = 16 * i + 2 * col + 1;
        }
        permute(&mut z.0, &idx);
    }
    if with_xor {
        next.xor_from(&r);
        next.xor_from(&z);
    } else {
        *next = r;
        next.xor_from(&z);
    }
}

/// Variable-length hash H' (RFC 9106 §3.3).
fn h_prime(out_len: usize, input: &[u8]) -> Vec<u8> {
    let mut pre = Vec::with_capacity(4 + input.len());
    pre.extend_from_slice(&(out_len as u32).to_le_bytes());
    pre.extend_from_slice(input);
    if out_len <= 64 {
        return blake2b(out_len, &pre);
    }
    let mut out = Vec::with_capacity(out_len);
    let mut v = blake2b(64, &pre);
    out.extend_from_slice(&v[..32]);
    let r = out_len.div_ceil(32) - 2;
    for _ in 1..r {
        v = blake2b(64, &v);
        out.extend_from_slice(&v[..32]);
    }
    v = blake2b(out_len - 32 * r, &v);
    out.extend_from_slice(&v);
    out
}

struct Argon2Instance<'a> {
    blocks: &'a mut [Block],
    lanes: usize,
    lane_len: usize,
    seg_len: usize,
    mprime: usize,
    t_cost: usize,
    variant: Argon2Variant,
    version: u32,
}

impl Argon2Instance<'_> {
    /// `address_block = G(0, G(0, input))`, bumping the counter word (RFC 9106 §3.4.1.2).
    fn next_addresses(input: &mut Block, addr: &mut Block) {
        input.0[6] += 1;
        let mut tmp = Block::ZERO;
        fill_block(&Block::ZERO, input, &mut tmp, false);
        fill_block(&Block::ZERO, &tmp, addr, false);
    }

    fn fill_segment(&mut self, pass: usize, slice: usize, lane: usize) {
        let di = self.variant == Argon2Variant::Argon2i
            || (self.variant == Argon2Variant::Argon2id && pass == 0 && slice < 2);
        let mut input = Block::ZERO;
        let mut addr = Block::ZERO;
        if di {
            input.0[0] = pass as u64;
            input.0[1] = lane as u64;
            input.0[2] = slice as u64;
            input.0[3] = self.mprime as u64;
            input.0[4] = self.t_cost as u64;
            input.0[5] = self.variant as u64;
        }
        let mut start = 0usize;
        if pass == 0 && slice == 0 {
            start = 2; // blocks 0 and 1 were seeded from H0
            if di {
                Self::next_addresses(&mut input, &mut addr);
            }
        }
        for i in start..self.seg_len {
            let curr = lane * self.lane_len + slice * self.seg_len + i;
            let prev = if curr % self.lane_len == 0 {
                curr + self.lane_len - 1
            } else {
                curr - 1
            };
            let rand64 = if di {
                if i % 128 == 0 {
                    Self::next_addresses(&mut input, &mut addr);
                }
                addr.0[i % 128]
            } else {
                self.blocks[prev].0[0]
            };
            let j1 = rand64 as u32 as u64;
            let j2 = (rand64 >> 32) as usize;
            let ref_lane = if pass == 0 && slice == 0 {
                lane
            } else {
                j2 % self.lanes
            };
            let same = ref_lane == lane;
            let ref_area = if pass == 0 {
                if slice == 0 {
                    i - 1
                } else if same {
                    slice * self.seg_len + i - 1
                } else {
                    slice * self.seg_len - usize::from(i == 0)
                }
            } else if same {
                self.lane_len - self.seg_len + i - 1
            } else {
                self.lane_len - self.seg_len - usize::from(i == 0)
            };
            let x = (j1 * j1) >> 32;
            let y = ((ref_area as u64) * x) >> 32;
            let z = (ref_area as u64) - 1 - y;
            let start_pos = if pass == 0 || slice == 3 {
                0
            } else {
                (slice + 1) * self.seg_len
            };
            let ref_index = (start_pos + z as usize) % self.lane_len;
            let prev_b = self.blocks[prev];
            let ref_b = self.blocks[ref_lane * self.lane_len + ref_index];
            let with_xor = pass > 0 && self.version >= 19;
            fill_block(&prev_b, &ref_b, &mut self.blocks[curr], with_xor);
        }
    }
}

/// The full Argon2 function, including the optional secret and associated-data inputs.
pub fn argon2_hash(password: &[u8], salt: &[u8], params: &Argon2Params) -> Vec<u8> {
    let lanes = params.lanes.max(1) as usize;
    let memory = params.m_cost.max(8 * params.lanes) as usize;
    let mprime = memory / (4 * lanes) * (4 * lanes);
    let lane_len = mprime / lanes;
    let seg_len = lane_len / 4;

    // H0 over the parameters and inputs (§3.2).
    let mut h0in = Vec::with_capacity(64 + password.len() + salt.len());
    for v in [
        params.lanes,
        params.out_len as u32,
        params.m_cost,
        params.t_cost,
        params.version,
        params.variant as u32,
    ] {
        h0in.extend_from_slice(&v.to_le_bytes());
    }
    h0in.extend_from_slice(&(password.len() as u32).to_le_bytes());
    h0in.extend_from_slice(password);
    h0in.extend_from_slice(&(salt.len() as u32).to_le_bytes());
    h0in.extend_from_slice(salt);
    h0in.extend_from_slice(&(params.secret.len() as u32).to_le_bytes());
    h0in.extend_from_slice(&params.secret);
    h0in.extend_from_slice(&(params.associated_data.len() as u32).to_le_bytes());
    h0in.extend_from_slice(&params.associated_data);
    let h0 = blake2b(64, &h0in);

    let mut blocks = vec![Block::ZERO; mprime];
    for l in 0..lanes {
        for j in 0..2usize {
            let mut seed = Vec::with_capacity(72);
            seed.extend_from_slice(&h0);
            seed.extend_from_slice(&(j as u32).to_le_bytes());
            seed.extend_from_slice(&(l as u32).to_le_bytes());
            blocks[l * lane_len + j] = Block::from_bytes(&h_prime(1024, &seed));
        }
    }

    let mut inst = Argon2Instance {
        blocks: &mut blocks,
        lanes,
        lane_len,
        seg_len,
        mprime,
        t_cost: params.t_cost as usize,
        variant: params.variant,
        version: params.version,
    };
    for pass in 0..params.t_cost as usize {
        for slice in 0..4usize {
            for lane in 0..lanes {
                inst.fill_segment(pass, slice, lane);
            }
        }
    }

    let mut c = blocks[lane_len - 1];
    for l in 1..lanes {
        c.xor_from(&blocks[l * lane_len + lane_len - 1]);
    }
    h_prime(params.out_len, &c.to_bytes())
}

// ---- bcrypt (OpenBSD EksBlowfish, `$2b$`) ------------------------------------------------------

struct Blowfish {
    p: [u32; 18],
    /// The four S-boxes, flattened: `s[box * 256 + index]`.
    s: [u32; 1024],
}

/// Next big-endian word from `data`, cycling (OpenBSD's `Blowfish_stream2word`).
fn stream2word(data: &[u8], j: &mut usize) -> u32 {
    let mut w = 0u32;
    for _ in 0..4 {
        w = (w << 8) | u32::from(data[*j]);
        *j = (*j + 1) % data.len();
    }
    w
}

impl Blowfish {
    fn new() -> Blowfish {
        Blowfish { p: BF_P, s: BF_S }
    }

    fn f(&self, x: u32) -> u32 {
        let a = self.s[(x >> 24) as usize];
        let b = self.s[256 + ((x >> 16) & 0xff) as usize];
        let c = self.s[512 + ((x >> 8) & 0xff) as usize];
        let d = self.s[768 + (x & 0xff) as usize];
        (a.wrapping_add(b) ^ c).wrapping_add(d)
    }

    fn encipher(&self, xl: &mut u32, xr: &mut u32) {
        let (mut l, mut r) = (*xl, *xr);
        for i in 0..16 {
            l ^= self.p[i];
            r ^= self.f(l);
            std::mem::swap(&mut l, &mut r);
        }
        std::mem::swap(&mut l, &mut r);
        r ^= self.p[16];
        l ^= self.p[17];
        *xl = l;
        *xr = r;
    }

    /// Salted key schedule (`Blowfish_expandstate`).
    fn expand_state(&mut self, salt: &[u8], key: &[u8]) {
        let mut j = 0usize;
        for i in 0..18 {
            self.p[i] ^= stream2word(key, &mut j);
        }
        let (mut l, mut r) = (0u32, 0u32);
        let mut k = 0usize;
        for i in (0..18).step_by(2) {
            l ^= stream2word(salt, &mut k);
            r ^= stream2word(salt, &mut k);
            self.encipher(&mut l, &mut r);
            self.p[i] = l;
            self.p[i + 1] = r;
        }
        for i in (0..1024).step_by(2) {
            l ^= stream2word(salt, &mut k);
            r ^= stream2word(salt, &mut k);
            self.encipher(&mut l, &mut r);
            self.s[i] = l;
            self.s[i + 1] = r;
        }
    }

    /// Plain key schedule (`Blowfish_expand0state`).
    fn expand0_state(&mut self, key: &[u8]) {
        let mut j = 0usize;
        for i in 0..18 {
            self.p[i] ^= stream2word(key, &mut j);
        }
        let (mut l, mut r) = (0u32, 0u32);
        for i in (0..18).step_by(2) {
            self.encipher(&mut l, &mut r);
            self.p[i] = l;
            self.p[i + 1] = r;
        }
        for i in (0..1024).step_by(2) {
            self.encipher(&mut l, &mut r);
            self.s[i] = l;
            self.s[i + 1] = r;
        }
    }
}

/// The 23 digest bytes a `$2b$` string encodes. Applies Bun's SHA-512 pre-hash for
/// passwords over 72 bytes, then the OpenBSD truncate-to-72-and-append-NUL key rule.
pub fn bcrypt_raw(password: &[u8], salt: &[u8; 16], cost: u32) -> [u8; 23] {
    let mut key: Vec<u8> = if password.len() > 72 {
        sha512(password).to_vec()
    } else {
        password.to_vec()
    };
    key.truncate(72);
    key.push(0);

    let mut bf = Blowfish::new();
    bf.expand_state(salt, &key);
    for _ in 0..(1u64 << cost) {
        bf.expand0_state(&key);
        bf.expand0_state(salt);
    }

    let magic = b"OrpheanBeholderScryDoubt";
    let mut cdata = [0u32; 6];
    let mut j = 0usize;
    for w in &mut cdata {
        *w = stream2word(magic, &mut j);
    }
    for _ in 0..64 {
        for b in 0..3 {
            let (mut l, mut r) = (cdata[2 * b], cdata[2 * b + 1]);
            bf.encipher(&mut l, &mut r);
            cdata[2 * b] = l;
            cdata[2 * b + 1] = r;
        }
    }
    let mut full = [0u8; 24];
    for (chunk, w) in full.chunks_exact_mut(4).zip(cdata) {
        chunk.copy_from_slice(&w.to_be_bytes());
    }
    let mut out = [0u8; 23];
    out.copy_from_slice(&full[..23]);
    out
}

// ---- base64 codecs -----------------------------------------------------------------------------

const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_BCRYPT: &[u8; 64] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Unpadded base64 over an arbitrary alphabet (PHC strings and bcrypt's crypt(3) flavor).
fn b64_encode(data: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[(n >> 18) as usize & 63] as char);
        out.push(alphabet[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(alphabet[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(alphabet[n as usize & 63] as char);
        }
    }
    out
}

fn b64_decode(s: &str, alphabet: &[u8; 64]) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let mut rev = [255u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.as_bytes().chunks(4) {
        let mut vals = [0u32; 4];
        for (v, &c) in vals.iter_mut().zip(chunk) {
            let d = rev[c as usize];
            if d == 255 {
                return None;
            }
            *v = u32::from(d);
        }
        let n = (vals[0] << 18) | (vals[1] << 12) | (vals[2] << 6) | vals[3];
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ---- PHC / $2b$ assembly and verification ------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PasswordError {
    UnsupportedAlgorithm,
    InvalidEncoding,
}

impl PasswordError {
    /// Bun's exact error message text (its `code` is derived from this in the JS glue).
    pub fn message(&self) -> &'static str {
        match self {
            PasswordError::UnsupportedAlgorithm => {
                "Password verification failed with error \"UnsupportedAlgorithm\""
            }
            PasswordError::InvalidEncoding => {
                "Password verification failed with error \"InvalidEncoding\""
            }
        }
    }
}

/// `$argon2X$v=19$m=..,t=..,p=1$salt$tag` exactly as Bun emits it (p=1, 32-byte tag).
pub fn argon2_phc(password: &[u8], salt: &[u8], m: u32, t: u32, variant: Argon2Variant) -> String {
    let params = Argon2Params {
        m_cost: m,
        t_cost: t,
        lanes: 1,
        variant,
        version: 19,
        out_len: 32,
        secret: Vec::new(),
        associated_data: Vec::new(),
    };
    let tag = argon2_hash(password, salt, &params);
    format!(
        "${}$v=19$m={},t={},p=1${}${}",
        variant.phc_name(),
        m,
        t,
        b64_encode(salt, B64_STD),
        b64_encode(&tag, B64_STD)
    )
}

/// `$2b$NN$<22 salt chars><31 digest chars>`.
pub fn bcrypt_string(password: &[u8], salt: &[u8; 16], cost: u32) -> String {
    let digest = bcrypt_raw(password, salt, cost);
    format!(
        "$2b${:02}${}{}",
        cost,
        b64_encode(salt, B64_BCRYPT),
        b64_encode(&digest, B64_BCRYPT)
    )
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn argon2_verify(password: &[u8], hash: &str) -> Result<bool, PasswordError> {
    let inv = PasswordError::InvalidEncoding;
    let mut it = hash.split('$');
    if it.next() != Some("") {
        return Err(inv);
    }
    let variant = it.next().and_then(Argon2Variant::from_phc).ok_or(inv)?;
    let mut part = it.next().ok_or(inv)?;
    let mut version = 19u32;
    if let Some(v) = part.strip_prefix("v=") {
        version = v.parse().map_err(|_| inv)?;
        if version != 19 && version != 16 {
            return Err(inv);
        }
        part = it.next().ok_or(inv)?;
    }
    let (mut m, mut t, mut p) = (None, None, None);
    for kv in part.split(',') {
        let (k, v) = kv.split_once('=').ok_or(inv)?;
        let val: u32 = v.parse().map_err(|_| inv)?;
        match k {
            "m" => m = Some(val),
            "t" => t = Some(val),
            "p" => p = Some(val),
            _ => return Err(inv),
        }
    }
    let (m, t, p) = (m.ok_or(inv)?, t.ok_or(inv)?, p.ok_or(inv)?);
    if m == 0 || t == 0 || p == 0 {
        return Err(inv);
    }
    let salt = b64_decode(it.next().ok_or(inv)?, B64_STD).ok_or(inv)?;
    let tag = b64_decode(it.next().ok_or(inv)?, B64_STD).ok_or(inv)?;
    if it.next().is_some() || salt.is_empty() || tag.len() < 4 {
        return Err(inv);
    }
    let params = Argon2Params {
        m_cost: m,
        t_cost: t,
        lanes: p,
        variant,
        version,
        out_len: tag.len(),
        secret: Vec::new(),
        associated_data: Vec::new(),
    };
    Ok(ct_eq(&argon2_hash(password, &salt, &params), &tag))
}

fn bcrypt_verify(password: &[u8], hash: &str) -> Result<bool, PasswordError> {
    let inv = PasswordError::InvalidEncoding;
    let b = hash.as_bytes();
    if b.len() != 60 || b[6] != b'$' {
        return Err(inv);
    }
    let cost: u32 = hash[4..6].parse().map_err(|_| inv)?;
    if !(4..=31).contains(&cost) {
        return Err(inv);
    }
    let salt: [u8; 16] = b64_decode(&hash[7..29], B64_BCRYPT)
        .ok_or(inv)?
        .try_into()
        .map_err(|_| inv)?;
    let expect = b64_decode(&hash[29..60], B64_BCRYPT).ok_or(inv)?;
    Ok(ct_eq(&bcrypt_raw(password, &salt, cost), &expect))
}

/// Verify `password` against a PHC argon2 string or a `$2[abxy]$` bcrypt string,
/// auto-detecting the algorithm exactly like Bun.
pub fn verify_password(password: &[u8], hash: &str) -> Result<bool, PasswordError> {
    if hash.starts_with("$argon2") {
        return argon2_verify(password, hash);
    }
    let b = hash.as_bytes();
    if b.len() > 3
        && b[0] == b'$'
        && b[1] == b'2'
        && matches!(b[2], b'a' | b'b' | b'x' | b'y')
        && b[3] == b'$'
    {
        return bcrypt_verify(password, hash);
    }
    Err(PasswordError::UnsupportedAlgorithm)
}

fn fill_random(buf: &mut [u8]) -> Result<(), String> {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| format!("read /dev/urandom: {e}"))
}

/// Hash with a fresh random salt. `algorithm` is one of bcrypt/argon2id/argon2i/argon2d;
/// the JS glue has already validated names and ranges with Bun's exact error messages —
/// the checks here are backstops.
pub fn hash_password(
    password: &[u8],
    algorithm: &str,
    m_cost: u32,
    t_cost: u32,
    cost: u32,
) -> Result<String, String> {
    match algorithm {
        "bcrypt" => {
            if !(4..=31).contains(&cost) {
                return Err("Rounds must be between 4 and 31".into());
            }
            let mut salt = [0u8; 16];
            fill_random(&mut salt)?;
            Ok(bcrypt_string(password, &salt, cost))
        }
        "argon2id" | "argon2i" | "argon2d" => {
            if t_cost == 0 {
                return Err("Time cost must be greater than 0".into());
            }
            if m_cost == 0 {
                return Err("Memory cost must be greater than 0".into());
            }
            let variant = Argon2Variant::from_phc(algorithm).expect("matched above");
            let mut salt = [0u8; 32];
            fill_random(&mut salt)?;
            Ok(argon2_phc(password, &salt, m_cost, t_cost, variant))
        }
        other => Err(format!("unknown password hashing algorithm: {other}")),
    }
}

// ---- ops (called only by the bun.js glue) ------------------------------------------------------

pub(crate) const PASSWORD_OPS: &[OpDecl] = ops![
    "hashSync" (5) => op_hash_sync,
    "verifySync" (2) => op_verify_sync,
    "hash" (7) => op_hash_async,
    "verify" (4) => op_verify_async,
    "argon2Sync" (9) => op_argon2_sync,
    "argon2" (11) => op_argon2_async,
];

fn arg_bytes(ctx: &mut Ctx, args: &[Value], i: usize, who: &str) -> Result<Vec<u8>, Value> {
    ctx.typed_array_bytes(args.get(i).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", format!("{who} expects Buffer bytes")))
}

fn arg_str(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.get(i).unwrap_or(&Value::Undefined))?
        .to_string())
}

/// Saturating f64 → u32 (the glue has already validated sign/type per Bun's messages).
fn arg_u32(args: &[Value], i: usize) -> u32 {
    let n = args.get(i).and_then(|v| v.as_num_opt()).unwrap_or(0.0);
    if n.is_nan() {
        0
    } else {
        n.clamp(0.0, u32::MAX as f64) as u32
    }
}

/// The trailing `(resolve, reject)` pair the glue passes; anything else is a glue bug.
fn settle_args(
    ctx: &mut Ctx,
    args: &[Value],
    i: usize,
    who: &str,
) -> Result<(Value, Value), Value> {
    match (args.get(i), args.get(i + 1)) {
        (Some(res), Some(rej)) if res.is_callable() && rej.is_callable() => {
            Ok((res.clone(), rej.clone()))
        }
        _ => Err(ctx.make_error("TypeError", format!("{who} expects (resolve, reject)"))),
    }
}

fn op_hash_sync(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let pw = arg_bytes(ctx, a, 0, "__password.hashSync")?;
    let alg = arg_str(ctx, a, 1)?;
    match hash_password(&pw, &alg, arg_u32(a, 2), arg_u32(a, 3), arg_u32(a, 4)) {
        Ok(s) => Ok(Value::from_string(s)),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

fn op_verify_sync(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let pw = arg_bytes(ctx, a, 0, "__password.verifySync")?;
    let hash = arg_str(ctx, a, 1)?;
    match verify_password(&pw, &hash) {
        Ok(b) => Ok(Value::Bool(b)),
        Err(e) => Err(ctx.make_error("Error", e.message())),
    }
}

fn op_hash_async(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let pw = arg_bytes(ctx, a, 0, "__password.hash")?;
    let alg = arg_str(ctx, a, 1)?;
    let (m, t, cost) = (arg_u32(a, 2), arg_u32(a, 3), arg_u32(a, 4));
    let (resolve, reject) = settle_args(ctx, a, 5, "__password.hash")?;
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(resolve, Some(reject), decode_hash);
    crate::spawn_handle(ctx)
        .spawn_blocking(id, move || Box::new(hash_password(&pw, &alg, m, t, cost)));
    Ok(Value::Undefined)
}

fn op_verify_async(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let pw = arg_bytes(ctx, a, 0, "__password.verify")?;
    let hash = arg_str(ctx, a, 1)?;
    let (resolve, reject) = settle_args(ctx, a, 2, "__password.verify")?;
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(resolve, Some(reject), decode_verify);
    crate::spawn_handle(ctx).spawn_blocking(id, move || {
        Box::new(verify_password(&pw, &hash).map_err(|e| e.message().to_string()))
    });
    Ok(Value::Undefined)
}

fn argon2_args(
    ctx: &mut Ctx,
    a: &[Value],
    who: &str,
) -> Result<(Vec<u8>, Vec<u8>, Argon2Params), Value> {
    let variant = Argon2Variant::from_phc(&arg_str(ctx, a, 0)?)
        .ok_or_else(|| ctx.make_error("TypeError", format!("{who}: invalid Argon2 algorithm")))?;
    let message = arg_bytes(ctx, a, 1, who)?;
    let nonce = arg_bytes(ctx, a, 2, who)?;
    let params = Argon2Params {
        lanes: arg_u32(a, 3),
        out_len: arg_u32(a, 4) as usize,
        m_cost: arg_u32(a, 5),
        t_cost: arg_u32(a, 6),
        variant,
        version: 19,
        secret: arg_bytes(ctx, a, 7, who)?,
        associated_data: arg_bytes(ctx, a, 8, who)?,
    };
    Ok((message, nonce, params))
}

fn op_argon2_sync(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let (message, nonce, params) = argon2_args(ctx, a, "crypto.argon2Sync")?;
    ctx.make_uint8array(&argon2_hash(&message, &nonce, &params))
}

fn op_argon2_async(ctx: &mut Ctx, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let (message, nonce, params) = argon2_args(ctx, a, "crypto.argon2")?;
    let (resolve, reject) = settle_args(ctx, a, 9, "crypto.argon2")?;
    let id = ctx
        .host_mut::<TaskRegistry>()
        .expect("runtime installs the registry")
        .register(resolve, Some(reject), decode_argon2);
    crate::spawn_handle(ctx)
        .spawn_blocking(id, move || Box::new(argon2_hash(&message, &nonce, &params)));
    Ok(Value::Undefined)
}

fn decode_argon2(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let bytes = *payload.downcast::<Vec<u8>>().expect("argon2 payload");
    Ok(vec![ctx.make_uint8array(&bytes)?])
}

fn decode_hash(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<String, String>>()
        .expect("hash payload")
    {
        Ok(s) => Ok(vec![Value::from_string(s)]),
        Err(m) => Err(ctx.make_error("Error", m)),
    }
}

fn decode_verify(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<bool, String>>()
        .expect("verify payload")
    {
        Ok(b) => Ok(vec![Value::Bool(b)]),
        Err(m) => Err(ctx.make_error("Error", m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha512_vectors() {
        // Expected digests from Python hashlib.
        assert_eq!(
            hex(&sha512(b"")),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            hex(&sha512(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        let p111: Vec<u8> = (0..111).collect();
        let p112: Vec<u8> = (0..112).collect();
        assert_eq!(
            hex(&sha512(&p111)),
            "a1a111449b198d9b1f538bad7f3fc1022b3a5b1a5e90a0bc860de8512746cbc31599e6c834de3a3235327af0b51ff57bf7acf1974a73014d9c3953812edc7c8d"
        );
        assert_eq!(
            hex(&sha512(&p112)),
            "c5fbd731d19d2ae1180f001be72c2c1aaba1d7b094b3748880e24593b8e117a750e11c1bd867cc2f96dace8c8b74abd2d5c4f236be444e77d30d1916174070b9"
        );
        assert_eq!(
            hex(&sha512(&b"xy".repeat(100))),
            "bc47d8689c66aaf83f2d5a1a38ebd81f5de17177527c78b85e8bfba617d6e38c012ab648ece7c9a56a1335cb1676f0431d0b5b6dc65fa535d22d3b676c2902c6"
        );
    }

    #[test]
    fn blake2b_vectors() {
        // Expected digests from Python hashlib.blake2b.
        assert_eq!(
            hex(&blake2b(64, b"")),
            "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce"
        );
        assert_eq!(
            hex(&blake2b(64, b"abc")),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
        );
        assert_eq!(
            hex(&blake2b(32, b"abc")),
            "bddd813c634239723171ef3fee98579b94964e3bb1cb3e427262c8c068d52319"
        );
        let p300: Vec<u8> = (0..300u32).map(|i| ((i * 7 + 3) % 256) as u8).collect();
        assert_eq!(
            hex(&blake2b(64, &p300)),
            "ec8fc8cb1343d5b8744a6971d9ae3b5463789bae1df407dadc65f7781d169a71596bc87f01691a81868f49acd6ddfbfd9874ab19eae83d40a505f6ca9db6af9f"
        );
        let p128: Vec<u8> = (0..128).collect();
        assert_eq!(
            hex(&blake2b(64, &p128)),
            "2319e3789c47e2daa5fe807f61bec2a1a6537fa03f19ff32e87eecbfd64b7e0e8ccff439ac333b040f19b0c4ddd11a61e24ac1fe0f10a039806c5dcc0da3d115"
        );
        assert_eq!(
            hex(&blake2b(17, &p128)),
            "7ca092935197a0c95bbc183b11cf4879f6"
        );
        // Incremental == one-shot across buffer boundaries.
        let mut inc = Blake2b::new(64);
        inc.update(&p300[..1]);
        inc.update(&p300[1..129]);
        inc.update(&p300[129..]);
        assert_eq!(inc.finalize(), blake2b(64, &p300));
    }

    #[test]
    fn blowfish_pi_constants() {
        // First P-array words are the leading hex digits of pi — a spot-check that the
        // generated tables are aligned correctly.
        assert_eq!(BF_P[0], 0x243f_6a88);
        assert_eq!(BF_P[1], 0x85a3_08d3);
        assert_eq!(BF_P[17], 0x8979_fb1b);
        assert_eq!(BF_S[0], 0xd131_0ba6);
    }

    #[test]
    fn b64_roundtrips() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len as u8)
                .map(|b| b.wrapping_mul(37).wrapping_add(9))
                .collect();
            for alphabet in [B64_STD, B64_BCRYPT] {
                let enc = b64_encode(&data, alphabet);
                assert_eq!(b64_decode(&enc, alphabet).as_deref(), Some(&data[..]));
            }
        }
        assert_eq!(b64_decode("a", B64_STD), None); // len % 4 == 1
        assert_eq!(b64_decode("!!!!", B64_STD), None); // bad chars
    }

    #[test]
    fn own_hashes_verify() {
        let salt32 = [7u8; 32];
        for variant in [
            Argon2Variant::Argon2d,
            Argon2Variant::Argon2i,
            Argon2Variant::Argon2id,
        ] {
            let phc = argon2_phc(b"s3cret", &salt32, 32, 2, variant);
            assert_eq!(verify_password(b"s3cret", &phc), Ok(true), "{phc}");
            assert_eq!(verify_password(b"wrong", &phc), Ok(false), "{phc}");
        }
        let salt16 = [9u8; 16];
        let bh = bcrypt_string(b"s3cret", &salt16, 4);
        assert!(bh.starts_with("$2b$04$") && bh.len() == 60, "{bh}");
        assert_eq!(verify_password(b"s3cret", &bh), Ok(true));
        assert_eq!(verify_password(b"wrong", &bh), Ok(false));
        // $2a/$2x/$2y aliases verify identically (Bun-compatible).
        for minor in ["a", "x", "y"] {
            let alias = format!("$2{minor}{}", &bh[3..]);
            assert_eq!(verify_password(b"s3cret", &alias), Ok(true), "{alias}");
        }
    }

    #[test]
    fn bcrypt_prehash_boundary() {
        // >72 bytes pre-hashes with SHA-512: hashing the long password and hashing its raw
        // digest must agree; 72 bytes must NOT be pre-hashed.
        let salt = [3u8; 16];
        let long = vec![b'A'; 100];
        assert_eq!(
            bcrypt_raw(&long, &salt, 4),
            bcrypt_raw(&sha512(&long), &salt, 4)
        );
        let pw72 = vec![b'A'; 72];
        assert_ne!(
            bcrypt_raw(&pw72, &salt, 4),
            bcrypt_raw(&sha512(&pw72), &salt, 4)
        );
    }

    #[test]
    fn bun_1_2_21_oracle_hashes_verify() {
        let fixtures: Vec<(Vec<u8>, &str)> = vec![
            (
                b"hunter2".to_vec(),
                "$argon2id$v=19$m=64,t=1,p=1$Qlq4y6N6W71yfwUALUE1saUFYrEf6EgHwYFtX0swMtU$3Zy/fZUAYcYmCSCCix/WJ0szZpIJI6SYjmGVcdfGtPw",
            ),
            (
                b"hunter2".to_vec(),
                "$argon2i$v=19$m=64,t=2,p=1$11HY6+bCFDRtMsONMVaCUvq4sISHSuc8ldW0Umyh3Mc$Mw+4ZZ6qKn8yvUNEUdE3FL5Zv2k37VsNNbgP7NAjafA",
            ),
            (
                b"hunter2".to_vec(),
                "$argon2d$v=19$m=64,t=2,p=1$g2sFB+MOMkV4ZtklRVjE+oaIPRRdaak2j8tMtt6Arss$ssJ/R8OCdo3od8rL8by1eheI0t7Yn2fYunxA3PDRlhY",
            ),
            (
                b"hunter2".to_vec(),
                "$2b$04$vVj0LFIN/tWPPW6l9KVfb./2Cr91.SdAmARN0zGOD41OBzylfgoGK",
            ),
            (
                vec![b'A'; 73],
                "$2b$04$VtsMUcJFE9ep38Tn9ECXQuyLfmvdzbZhhQU6r.bm.ByAvuWAAVSC6",
            ),
            (
                vec![0, 255, 128, 1, 170, 85],
                "$2b$04$jz6swG09WNYeIYtvADKPH.jmlAMlOvFuuTPm53ndjH0GiNSPbaty.",
            ),
        ];

        for (password, hash) in fixtures {
            assert_eq!(verify_password(&password, hash), Ok(true), "{hash}");
            assert_eq!(verify_password(b"wrong", hash), Ok(false), "{hash}");
        }
    }

    #[test]
    fn argon2_memory_clamp_matches_requested_m_in_h0_only() {
        // m below 8*lanes clamps the matrix but the requested m feeds H0, so m=1 and m=8
        // differ (m is in H0) while both use an 8-block matrix — this is Zig's (Bun's)
        // behavior, cross-checked by the m=1/m=7 oracle fixtures.
        let params = |m: u32| Argon2Params {
            m_cost: m,
            t_cost: 1,
            lanes: 1,
            variant: Argon2Variant::Argon2id,
            version: 19,
            out_len: 32,
            secret: Vec::new(),
            associated_data: Vec::new(),
        };
        let a = argon2_hash(b"pw", &[1u8; 16], &params(1));
        let b = argon2_hash(b"pw", &[1u8; 16], &params(8));
        assert_ne!(a, b);
    }

    #[test]
    fn argon2id_rfc9106_secret_and_associated_data_vector() {
        let params = Argon2Params {
            m_cost: 32,
            t_cost: 3,
            lanes: 4,
            variant: Argon2Variant::Argon2id,
            version: 19,
            out_len: 32,
            secret: vec![3; 8],
            associated_data: vec![4; 12],
        };
        assert_eq!(
            hex(&argon2_hash(&vec![1; 32], &vec![2; 16], &params)),
            "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659"
        );
    }

    #[test]
    fn verify_error_shapes() {
        assert_eq!(
            verify_password(b"x", "not-a-hash"),
            Err(PasswordError::UnsupportedAlgorithm)
        );
        assert_eq!(
            verify_password(b"x", "$argon2id$v=19$m=64,t=2,p=1$!!bad!!$tag"),
            Err(PasswordError::InvalidEncoding)
        );
        assert_eq!(
            verify_password(b"x", "$argon2id$v=19$m=64,t=0,p=1$AAAAAAAA$AAAAAAAA"),
            Err(PasswordError::InvalidEncoding)
        );
        assert_eq!(
            verify_password(
                b"x",
                "$2b$99$......................!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
            ),
            Err(PasswordError::InvalidEncoding)
        );
        assert_eq!(
            verify_password(b"x", "$2b$04$tooshort"),
            Err(PasswordError::InvalidEncoding)
        );
    }
}
