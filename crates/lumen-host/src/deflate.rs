//! DEFLATE (RFC 1951) with zlib (RFC 1950) and gzip (RFC 1952) framing — a from-scratch, std-only
//! codec shared by the web `CompressionStream`/`DecompressionStream` and `node:zlib`. No external
//! crates: inflate handles stored/fixed/dynamic Huffman blocks; deflate emits fixed-Huffman blocks
//! with greedy LZ77 matching (real compression, not stored-only). Checksums: Adler-32 (zlib) and
//! CRC-32 (gzip).

// ---- checksums --------------------------------------------------------------------------------

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

pub fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

// ---- bit reader (LSB-first, per DEFLATE) ------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit_buf: u32,
    bit_cnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0, bit_buf: 0, bit_cnt: 0 }
    }
    fn bit(&mut self) -> Result<u32, String> {
        if self.bit_cnt == 0 {
            if self.pos >= self.data.len() {
                return Err("inflate: unexpected end of input".into());
            }
            self.bit_buf = self.data[self.pos] as u32;
            self.pos += 1;
            self.bit_cnt = 8;
        }
        let b = self.bit_buf & 1;
        self.bit_buf >>= 1;
        self.bit_cnt -= 1;
        Ok(b)
    }
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }
    fn align_to_byte(&mut self) {
        self.bit_buf = 0;
        self.bit_cnt = 0;
    }
}

// ---- Huffman decoding -------------------------------------------------------------------------

/// Canonical Huffman decode table built from per-symbol code lengths.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &len in lengths {
            counts[len as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for i in 1..16 {
            offsets[i] = offsets[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offsets[len as usize] as usize] = sym as u16;
                offsets[len as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }
    fn decode(&self, reader: &mut BitReader) -> Result<u16, String> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= reader.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("inflate: invalid Huffman code".into())
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_huffman() -> (Huffman, Huffman) {
    let mut lit_lengths = [0u8; 288];
    for (i, len) in lit_lengths.iter_mut().enumerate() {
        *len = if i < 144 {
            8
        } else if i < 256 {
            9
        } else if i < 280 {
            7
        } else {
            8
        };
    }
    let dist_lengths = [5u8; 30];
    (Huffman::new(&lit_lengths), Huffman::new(&dist_lengths))
}

fn inflate_block(
    reader: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(reader)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()), // end of block
            257..=285 => {
                let i = (sym - 257) as usize;
                let length =
                    LENGTH_BASE[i] as usize + reader.bits(LENGTH_EXTRA[i] as u32)? as usize;
                let dsym = dist.decode(reader)? as usize;
                if dsym >= 30 {
                    return Err("inflate: invalid distance symbol".into());
                }
                let distance =
                    DIST_BASE[dsym] as usize + reader.bits(DIST_EXTRA[dsym] as u32)? as usize;
                if distance > out.len() {
                    return Err("inflate: distance too far back".into());
                }
                let start = out.len() - distance;
                for k in 0..length {
                    out.push(out[start + k]);
                }
            }
            _ => return Err("inflate: invalid literal/length symbol".into()),
        }
    }
}

/// Decode raw DEFLATE (no zlib/gzip wrapper).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut reader = BitReader::new(data);
    let mut out = Vec::new();
    loop {
        let final_block = reader.bit()?;
        let btype = reader.bits(2)?;
        match btype {
            0 => {
                reader.align_to_byte();
                if reader.pos + 4 > data.len() {
                    return Err("inflate: truncated stored block".into());
                }
                let len = data[reader.pos] as usize | ((data[reader.pos + 1] as usize) << 8);
                reader.pos += 4; // LEN + NLEN
                if reader.pos + len > data.len() {
                    return Err("inflate: stored block overruns input".into());
                }
                out.extend_from_slice(&data[reader.pos..reader.pos + len]);
                reader.pos += len;
            }
            1 => {
                let (lit, dist) = fixed_huffman();
                inflate_block(&mut reader, &mut out, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = read_dynamic_tables(&mut reader)?;
                inflate_block(&mut reader, &mut out, &lit, &dist)?;
            }
            _ => return Err("inflate: reserved block type".into()),
        }
        if final_block == 1 {
            return Ok(out);
        }
    }
}

const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn read_dynamic_tables(reader: &mut BitReader) -> Result<(Huffman, Huffman), String> {
    let hlit = reader.bits(5)? as usize + 257;
    let hdist = reader.bits(5)? as usize + 1;
    let hclen = reader.bits(4)? as usize + 4;

    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[CODE_LENGTH_ORDER[i]] = reader.bits(3)? as u8;
    }
    let cl_huffman = Huffman::new(&cl_lengths);

    let mut lengths = Vec::with_capacity(hlit + hdist);
    while lengths.len() < hlit + hdist {
        let sym = cl_huffman.decode(reader)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths.last().ok_or("inflate: repeat with no previous length")?;
                for _ in 0..(reader.bits(2)? + 3) {
                    lengths.push(prev);
                }
            }
            17 => {
                for _ in 0..(reader.bits(3)? + 3) {
                    lengths.push(0);
                }
            }
            18 => {
                for _ in 0..(reader.bits(7)? + 11) {
                    lengths.push(0);
                }
            }
            _ => return Err("inflate: invalid code-length symbol".into()),
        }
    }
    if lengths.len() > hlit + hdist {
        return Err("inflate: code-length overrun".into());
    }
    let (lit_lengths, dist_lengths) = lengths.split_at(hlit);
    Ok((Huffman::new(lit_lengths), Huffman::new(dist_lengths)))
}

// ---- deflate encoder (fixed Huffman + greedy LZ77) --------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    bit_buf: u32,
    bit_cnt: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), bit_buf: 0, bit_cnt: 0 }
    }
    fn write(&mut self, value: u32, n: u32) {
        self.bit_buf |= value << self.bit_cnt;
        self.bit_cnt += n;
        while self.bit_cnt >= 8 {
            self.out.push((self.bit_buf & 0xff) as u8);
            self.bit_buf >>= 8;
            self.bit_cnt -= 8;
        }
    }
    /// Huffman codes are written MSB-first (bit-reversed relative to the LSB bit order).
    fn write_code(&mut self, code: u32, n: u32) {
        let mut reversed = 0;
        for i in 0..n {
            reversed |= ((code >> i) & 1) << (n - 1 - i);
        }
        self.write(reversed, n);
    }
    fn finish(mut self) -> Vec<u8> {
        if self.bit_cnt > 0 {
            self.out.push((self.bit_buf & 0xff) as u8);
        }
        self.out
    }
}

/// Fixed-Huffman literal/length code for a symbol (0..=287) — code value and bit length.
fn fixed_lit_code(sym: u16) -> (u32, u32) {
    match sym {
        0..=143 => (0x30 + sym as u32, 8),
        144..=255 => (0x190 + (sym as u32 - 144), 9),
        256..=279 => (sym as u32 - 256, 7),
        _ => (0xc0 + (sym as u32 - 280), 8),
    }
}

fn length_symbol(length: usize) -> (u16, u32, u32) {
    for i in (0..29).rev() {
        if length >= LENGTH_BASE[i] as usize {
            let extra = length - LENGTH_BASE[i] as usize;
            return (257 + i as u16, extra as u32, LENGTH_EXTRA[i] as u32);
        }
    }
    (257, 0, 0)
}

fn dist_symbol(distance: usize) -> (u16, u32, u32) {
    for i in (0..30).rev() {
        if distance >= DIST_BASE[i] as usize {
            let extra = distance - DIST_BASE[i] as usize;
            return (i as u16, extra as u32, DIST_EXTRA[i] as u32);
        }
    }
    (0, 0, 0)
}

const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;

fn hash3(data: &[u8], i: usize) -> usize {
    let v = (data[i] as usize) << 16 | (data[i + 1] as usize) << 8 | data[i + 2] as usize;
    (v.wrapping_mul(2654435761)) >> (32 - HASH_BITS) & (HASH_SIZE - 1)
}

/// Encode raw DEFLATE: one fixed-Huffman block with greedy LZ77 (hash-chain match finder).
pub fn deflate(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write(1, 1); // BFINAL = 1 (single block)
    w.write(1, 2); // BTYPE = 01 (fixed Huffman)

    let emit_literal = |w: &mut BitWriter, byte: u8| {
        let (code, len) = fixed_lit_code(byte as u16);
        w.write_code(code, len);
    };

    let n = data.len();
    let mut head = vec![usize::MAX; HASH_SIZE];
    let mut prev = vec![usize::MAX; n.max(1)];
    let mut i = 0;
    while i < n {
        let mut best_len = 0;
        let mut best_dist = 0;
        if i + MIN_MATCH <= n {
            let h = hash3(data, i);
            let mut cand = head[h];
            let mut chain = 0;
            while cand != usize::MAX && chain < 128 {
                let max_len = (n - i).min(MAX_MATCH);
                let mut len = 0;
                while len < max_len && data[cand + len] == data[i + len] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = i - cand;
                    if len >= max_len {
                        break;
                    }
                }
                cand = prev[cand];
                chain += 1;
            }
            prev[i] = head[h];
            head[h] = i;
        }

        if best_len >= MIN_MATCH {
            let (lsym, lextra, lbits) = length_symbol(best_len);
            let (lcode, lcodelen) = fixed_lit_code(lsym);
            w.write_code(lcode, lcodelen);
            if lbits > 0 {
                w.write(lextra, lbits);
            }
            let (dsym, dextra, dbits) = dist_symbol(best_dist);
            w.write_code(dsym as u32, 5);
            if dbits > 0 {
                w.write(dextra, dbits);
            }
            // Insert hash entries for the bytes the match covers (skip the first, already inserted).
            let end = i + best_len;
            let mut j = i + 1;
            while j < end && j + MIN_MATCH <= n {
                let h = hash3(data, j);
                prev[j] = head[h];
                head[h] = j;
                j += 1;
            }
            i = end;
        } else {
            emit_literal(&mut w, data[i]);
            i += 1;
        }
    }
    // End-of-block symbol (256).
    let (code, len) = fixed_lit_code(256);
    w.write_code(code, len);
    w.finish()
}

// ---- zlib / gzip framing ----------------------------------------------------------------------

pub fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x9c]; // CMF/FLG (deflate, default window, default level)
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 6 {
        return Err("zlib: input too short".into());
    }
    if data[0] & 0x0f != 8 {
        return Err("zlib: unsupported compression method".into());
    }
    let out = inflate(&data[2..])?;
    Ok(out)
}

pub fn gzip_compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]; // magic, method, flags, mtime, xfl, os
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b {
        return Err("gzip: bad magic".into());
    }
    if data[2] != 8 {
        return Err("gzip: unsupported compression method".into());
    }
    let flags = data[3];
    let mut pos = 10;
    if flags & 0x04 != 0 {
        // FEXTRA
        if pos + 2 > data.len() {
            return Err("gzip: truncated extra field".into());
        }
        let xlen = data[pos] as usize | ((data[pos + 1] as usize) << 8);
        pos += 2 + xlen;
    }
    if flags & 0x08 != 0 {
        // FNAME (NUL-terminated)
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    if flags & 0x10 != 0 {
        // FCOMMENT
        while pos < data.len() && data[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    if flags & 0x02 != 0 {
        pos += 2; // FHCRC
    }
    if pos + 8 > data.len() {
        return Err("gzip: truncated".into());
    }
    inflate(&data[pos..data.len() - 8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        assert_eq!(inflate(&deflate(data)).unwrap(), data, "raw deflate");
        assert_eq!(zlib_decompress(&zlib_compress(data)).unwrap(), data, "zlib");
        assert_eq!(gzip_decompress(&gzip_compress(data)).unwrap(), data, "gzip");
    }

    #[test]
    fn roundtrips() {
        roundtrip(b"");
        roundtrip(b"a");
        roundtrip(b"hello, hello, hello world!");
        roundtrip(&[0u8; 1000]); // long run — exercises back-references
        let repetitive: Vec<u8> = (0..5000).map(|i| (i % 7) as u8).collect();
        roundtrip(&repetitive);
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(50);
        roundtrip(text.as_bytes());
    }

    #[test]
    fn compresses_repetitive_input() {
        let data = "abcabcabcabc".repeat(100);
        let compressed = deflate(data.as_bytes());
        assert!(compressed.len() < data.len() / 2, "expected real compression");
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E60398);
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }
}
