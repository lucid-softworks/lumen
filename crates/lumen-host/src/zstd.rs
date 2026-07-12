//! Zstandard (RFC 8878) — a from-scratch, std-only codec. The decoder implements the complete
//! format: frame headers, raw/RLE/compressed blocks, FSE table construction and decoding, Huffman
//! literals (direct and FSE-compressed weights, single- and 4-stream), the sequences section with
//! all four table modes (predefined / RLE / FSE / repeat) and the repeat-offset history, plus
//! XXH64 content-checksum verification. The encoder emits valid frames from raw/RLE blocks with a
//! content checksum (honest framing; it does not attempt FSE/Huffman entropy coding). Dictionary
//! frames are rejected with an error.

const ZSTD_MAGIC: u32 = 0xFD2F_B528;
const MAX_BLOCK_SIZE: usize = 128 * 1024;

// ---- XXH64 -------------------------------------------------------------------------------------

const XXH_P1: u64 = 0x9E37_79B1_85EB_CA87;
const XXH_P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const XXH_P3: u64 = 0x1656_67B1_9E37_79F9;
const XXH_P4: u64 = 0x85EB_CA77_C2B2_AE63;
const XXH_P5: u64 = 0x27D4_EB2F_1656_67C5;

fn xxh_round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(XXH_P2)).rotate_left(31).wrapping_mul(XXH_P1)
}

pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let le64 = |s: &[u8]| u64::from_le_bytes(s[..8].try_into().unwrap());
    let le32 = |s: &[u8]| u32::from_le_bytes(s[..4].try_into().unwrap());
    let mut pos = 0;
    let mut acc: u64;
    if data.len() >= 32 {
        let mut v = [
            seed.wrapping_add(XXH_P1).wrapping_add(XXH_P2),
            seed.wrapping_add(XXH_P2),
            seed,
            seed.wrapping_sub(XXH_P1),
        ];
        while pos + 32 <= data.len() {
            for (i, vi) in v.iter_mut().enumerate() {
                *vi = xxh_round(*vi, le64(&data[pos + 8 * i..]));
            }
            pos += 32;
        }
        acc = v[0]
            .rotate_left(1)
            .wrapping_add(v[1].rotate_left(7))
            .wrapping_add(v[2].rotate_left(12))
            .wrapping_add(v[3].rotate_left(18));
        for vi in v {
            acc = (acc ^ xxh_round(0, vi)).wrapping_mul(XXH_P1).wrapping_add(XXH_P4);
        }
    } else {
        acc = seed.wrapping_add(XXH_P5);
    }
    acc = acc.wrapping_add(data.len() as u64);
    while pos + 8 <= data.len() {
        acc ^= xxh_round(0, le64(&data[pos..]));
        acc = acc.rotate_left(27).wrapping_mul(XXH_P1).wrapping_add(XXH_P4);
        pos += 8;
    }
    if pos + 4 <= data.len() {
        acc ^= (le32(&data[pos..]) as u64).wrapping_mul(XXH_P1);
        acc = acc.rotate_left(23).wrapping_mul(XXH_P2).wrapping_add(XXH_P3);
        pos += 4;
    }
    for &b in &data[pos..] {
        acc ^= (b as u64).wrapping_mul(XXH_P5);
        acc = acc.rotate_left(11).wrapping_mul(XXH_P1);
    }
    acc ^= acc >> 33;
    acc = acc.wrapping_mul(XXH_P2);
    acc ^= acc >> 29;
    acc = acc.wrapping_mul(XXH_P3);
    acc ^= acc >> 32;
    acc
}

// ---- bit readers -------------------------------------------------------------------------------

/// Forward LSB-first bit reader, used for FSE table descriptions. Reads past the end are
/// zero-filled; callers must validate `bytes_consumed()` against the available region.
struct FwdBits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> FwdBits<'a> {
    fn new(data: &'a [u8]) -> Self {
        FwdBits { data, pos: 0 }
    }
    fn peek(&self, n: u32) -> u32 {
        let mut v = 0u32;
        for k in 0..n as usize {
            let j = self.pos + k;
            if j < self.data.len() * 8 {
                let bit = (self.data[j >> 3] >> (j & 7)) & 1;
                v |= (bit as u32) << k;
            }
        }
        v
    }
    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }
    fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }
    fn bytes_consumed(&self) -> usize {
        self.pos.div_ceil(8)
    }
}

/// Backward bitstream reader (FSE / Huffman streams). The stream is read starting from the last
/// byte, whose highest set bit is a sentinel marking the end of padding. Reads below position 0
/// are zero-filled and flip the stream into the "overflowed" state.
struct RevBits<'a> {
    data: &'a [u8],
    pos: i64, // number of valid bits remaining
}

impl<'a> RevBits<'a> {
    fn new(data: &'a [u8]) -> Result<Self, String> {
        let last = *data.last().ok_or("zstd: empty bitstream")?;
        if last == 0 {
            return Err("zstd: bitstream missing sentinel bit".into());
        }
        let pos = data.len() as i64 * 8 - i64::from(last.leading_zeros()) - 1;
        Ok(RevBits { data, pos })
    }
    fn peek(&self, n: u32) -> u64 {
        let mut v = 0u64;
        let lo = self.pos - i64::from(n);
        for k in 0..i64::from(n) {
            let j = lo + k;
            if j >= 0 {
                let bit = (self.data[(j >> 3) as usize] >> (j & 7)) & 1;
                v |= u64::from(bit) << k;
            }
        }
        v
    }
    fn skip(&mut self, n: u32) {
        self.pos -= i64::from(n);
    }
    fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.skip(n);
        v
    }
    fn overflowed(&self) -> bool {
        self.pos < 0
    }
    fn finished(&self) -> bool {
        self.pos == 0
    }
}

// ---- FSE ---------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct FseCell {
    symbol: u8,
    nbits: u8,
    base: u16,
}

#[derive(Clone)]
struct FseTable {
    log: u32,
    cells: Vec<FseCell>,
}

impl FseTable {
    /// Single-symbol table for RLE mode: accuracy log 0, every decode yields `symbol`.
    fn rle(symbol: u8) -> FseTable {
        FseTable { log: 0, cells: vec![FseCell { symbol, nbits: 0, base: 0 }] }
    }
    fn cell(&self, state: usize) -> Result<FseCell, String> {
        self.cells.get(state).copied().ok_or_else(|| "zstd: FSE state out of range".to_string())
    }
}

/// Parse a normalized-count distribution (RFC 8878 §4.1.1). Returns the probabilities (-1 means
/// "less than 1") and the accuracy log.
fn fse_read_distribution(
    fwd: &mut FwdBits,
    max_log: u32,
    max_symbols: usize,
) -> Result<(Vec<i32>, u32), String> {
    let log = fwd.read(4) + 5;
    if log > max_log {
        return Err("zstd: FSE accuracy log too large".into());
    }
    let table_size = 1i32 << log;
    let mut remaining = table_size + 1;
    let mut threshold = table_size;
    let mut nbits = log + 1;
    let mut probs: Vec<i32> = Vec::new();
    while remaining > 1 {
        if probs.len() > max_symbols {
            return Err("zstd: too many symbols in FSE distribution".into());
        }
        let max = (2 * threshold - 1) - remaining;
        let peek = fwd.peek(nbits) as i32;
        let low = peek & (threshold - 1);
        let mut count = if low < max {
            fwd.skip(nbits - 1);
            low
        } else {
            fwd.skip(nbits);
            let mut c = peek & (2 * threshold - 1);
            if c >= threshold {
                c -= max;
            }
            c
        };
        count -= 1; // stored value is probability + 1; -1 encodes "less than 1"
        remaining -= count.abs();
        probs.push(count);
        if count == 0 {
            // A zero probability is followed by 2-bit runs of extra zeros; 3 means "keep going".
            loop {
                let rep = fwd.read(2);
                for _ in 0..rep {
                    probs.push(0);
                }
                if rep < 3 {
                    break;
                }
                if probs.len() > max_symbols {
                    return Err("zstd: too many symbols in FSE distribution".into());
                }
            }
        }
        while remaining > 1 && remaining < threshold {
            threshold >>= 1;
            nbits -= 1;
        }
    }
    if remaining != 1 || probs.len() > max_symbols {
        return Err("zstd: corrupted FSE distribution".into());
    }
    Ok((probs, log))
}

/// Build a decoding table from normalized probabilities (RFC 8878 §4.1.1.2).
fn fse_build(probs: &[i32], log: u32) -> Result<FseTable, String> {
    let size = 1usize << log;
    let total: i64 = probs.iter().map(|&p| if p == -1 { 1 } else { i64::from(p.max(0)) }).sum();
    if total != size as i64 {
        return Err("zstd: FSE probabilities do not sum to table size".into());
    }
    let mut symbols = vec![0u8; size];
    // "Less than 1" symbols each take one cell at the very end of the table.
    let mut high = size as i64 - 1;
    for (s, &p) in probs.iter().enumerate() {
        if p == -1 {
            symbols[high as usize] = s as u8;
            high -= 1;
        }
    }
    // Spread the remaining symbols with the standard step, skipping the low-probability region.
    let step = (size >> 1) + (size >> 3) + 3;
    let mask = size - 1;
    let mut pos = 0usize;
    for (s, &p) in probs.iter().enumerate() {
        for _ in 0..p.max(0) {
            symbols[pos] = s as u8;
            loop {
                pos = (pos + step) & mask;
                if pos as i64 <= high {
                    break;
                }
            }
        }
    }
    if pos != 0 {
        return Err("zstd: corrupted FSE table spread".into());
    }
    let mut next: Vec<u32> = probs.iter().map(|&p| if p < 0 { 1 } else { p as u32 }).collect();
    let mut cells = Vec::with_capacity(size);
    for &sym in &symbols {
        let x = next[sym as usize];
        next[sym as usize] += 1;
        let nbits = log - (31 - x.leading_zeros());
        let base = ((x as usize) << nbits) - size;
        cells.push(FseCell { symbol: sym, nbits: nbits as u8, base: base as u16 });
    }
    Ok(FseTable { log, cells })
}

// ---- Huffman -----------------------------------------------------------------------------------

#[derive(Clone)]
struct HufTable {
    log: u32,
    cells: Vec<(u8, u8)>, // (symbol, code length) for every `log`-bit prefix
}

/// Parse a Huffman tree description (direct or FSE-compressed weights) and return the decode
/// table plus the number of header bytes consumed.
fn huf_read_table(data: &[u8]) -> Result<(HufTable, usize), String> {
    let h = *data.first().ok_or("zstd: missing Huffman tree header")? as usize;
    let mut weights: Vec<u8> = Vec::new();
    let consumed;
    if h >= 128 {
        // Direct representation: (h - 127) weights, 4 bits each.
        let n = h - 127;
        let nbytes = n.div_ceil(2);
        if 1 + nbytes > data.len() {
            return Err("zstd: truncated Huffman weights".into());
        }
        for i in 0..n {
            let b = data[1 + i / 2];
            weights.push(if i % 2 == 0 { b >> 4 } else { b & 0x0F });
        }
        consumed = 1 + nbytes;
    } else {
        // FSE-compressed weights over an `h`-byte region.
        if 1 + h > data.len() {
            return Err("zstd: truncated Huffman tree description".into());
        }
        let region = &data[1..1 + h];
        let mut fwd = FwdBits::new(region);
        let (probs, log) = fse_read_distribution(&mut fwd, 6, 255)?;
        let table = fse_build(&probs, log)?;
        let used = fwd.bytes_consumed();
        if used >= region.len() {
            return Err("zstd: truncated Huffman weights bitstream".into());
        }
        let mut rev = RevBits::new(&region[used..])?;
        let mut s1 = rev.read(log) as usize;
        let mut s2 = rev.read(log) as usize;
        if rev.overflowed() {
            return Err("zstd: truncated Huffman weights bitstream".into());
        }
        // The two states alternate; when a state update overruns the stream, emit one final
        // symbol from the other state and stop (RFC 8878 §4.2.1.3).
        loop {
            if weights.len() > 253 {
                return Err("zstd: too many Huffman weights".into());
            }
            let c = table.cell(s1)?;
            weights.push(c.symbol);
            s1 = c.base as usize + rev.read(u32::from(c.nbits)) as usize;
            if rev.overflowed() {
                weights.push(table.cell(s2)?.symbol);
                break;
            }
            let c = table.cell(s2)?;
            weights.push(c.symbol);
            s2 = c.base as usize + rev.read(u32::from(c.nbits)) as usize;
            if rev.overflowed() {
                weights.push(table.cell(s1)?.symbol);
                break;
            }
        }
        consumed = 1 + h;
    }
    Ok((huf_from_weights(&weights)?, consumed))
}

/// Build the Huffman decode table from explicit weights; the final weight is implied (it
/// completes the weight sum to a power of two).
fn huf_from_weights(weights: &[u8]) -> Result<HufTable, String> {
    if weights.len() > 255 {
        return Err("zstd: too many Huffman weights".into());
    }
    let mut sum: u64 = 0;
    for &w in weights {
        if w > 11 {
            return Err("zstd: Huffman weight too large".into());
        }
        if w > 0 {
            sum += 1u64 << (w - 1);
        }
    }
    if sum == 0 {
        return Err("zstd: empty Huffman table".into());
    }
    let max_bits = 64 - sum.leading_zeros(); // smallest power of two strictly above `sum`
    if max_bits > 11 {
        return Err("zstd: Huffman code length too large".into());
    }
    let rest = (1u64 << max_bits) - sum;
    if !rest.is_power_of_two() {
        return Err("zstd: corrupted Huffman weights".into());
    }
    let last_weight = rest.trailing_zeros() as u8 + 1;
    let mut all = weights.to_vec();
    all.push(last_weight);

    let size = 1usize << max_bits;
    let mut cells = vec![(0u8, 0u8); size];
    let mut pos = 0usize;
    // Canonical assignment: ascending weight (descending code length), natural symbol order.
    for w in 1..=max_bits as u8 {
        let nbits = max_bits as u8 + 1 - w;
        let count = 1usize << (w - 1);
        for (sym, &wt) in all.iter().enumerate() {
            if wt == w {
                if pos + count > size {
                    return Err("zstd: corrupted Huffman weights".into());
                }
                for c in &mut cells[pos..pos + count] {
                    *c = (sym as u8, nbits);
                }
                pos += count;
            }
        }
    }
    if pos != size {
        return Err("zstd: corrupted Huffman weights".into());
    }
    Ok(HufTable { log: max_bits, cells })
}

fn huf_decode_stream(
    table: &HufTable,
    stream: &[u8],
    count: usize,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let mut rev = RevBits::new(stream)?;
    for _ in 0..count {
        let idx = rev.peek(table.log) as usize;
        let (sym, nbits) = table.cells[idx];
        rev.skip(u32::from(nbits));
        if rev.overflowed() {
            return Err("zstd: Huffman stream overrun".into());
        }
        out.push(sym);
    }
    if !rev.finished() {
        return Err("zstd: Huffman stream not fully consumed".into());
    }
    Ok(())
}

// ---- literals section --------------------------------------------------------------------------

/// Decode the literals section of a compressed block. Returns the literals and the number of
/// block bytes consumed.
fn decode_literals(block: &[u8], ctx: &mut FrameCtx) -> Result<(Vec<u8>, usize), String> {
    let b0 = *block.first().ok_or("zstd: missing literals section header")? as usize;
    let lit_type = b0 & 3;
    let size_format = (b0 >> 2) & 3;
    let need = |n: usize| -> Result<(), String> {
        if n > block.len() { Err("zstd: truncated literals section".into()) } else { Ok(()) }
    };
    if lit_type <= 1 {
        // Raw or RLE literals.
        let (regen, hlen) = match size_format {
            0 | 2 => (b0 >> 3, 1),
            1 => {
                need(2)?;
                ((b0 >> 4) | ((block[1] as usize) << 4), 2)
            }
            _ => {
                need(3)?;
                ((b0 >> 4) | ((block[1] as usize) << 4) | ((block[2] as usize) << 12), 3)
            }
        };
        if regen > MAX_BLOCK_SIZE {
            return Err("zstd: literals section too large".into());
        }
        return if lit_type == 0 {
            need(hlen + regen)?;
            Ok((block[hlen..hlen + regen].to_vec(), hlen + regen))
        } else {
            need(hlen + 1)?;
            Ok((vec![block[hlen]; regen], hlen + 1))
        };
    }

    // Compressed (2) or treeless (3) literals.
    let (regen, comp, four_streams, hlen) = match size_format {
        0 | 1 => {
            need(3)?;
            let v = b0 | ((block[1] as usize) << 8) | ((block[2] as usize) << 16);
            ((v >> 4) & 0x3FF, (v >> 14) & 0x3FF, size_format == 1, 3)
        }
        2 => {
            need(4)?;
            let v = b0
                | ((block[1] as usize) << 8)
                | ((block[2] as usize) << 16)
                | ((block[3] as usize) << 24);
            ((v >> 4) & 0x3FFF, (v >> 18) & 0x3FFF, true, 4)
        }
        _ => {
            need(5)?;
            let v = (b0 as u64)
                | ((block[1] as u64) << 8)
                | ((block[2] as u64) << 16)
                | ((block[3] as u64) << 24)
                | ((block[4] as u64) << 32);
            (((v >> 4) & 0x3FFFF) as usize, ((v >> 22) & 0x3FFFF) as usize, true, 5)
        }
    };
    if regen > MAX_BLOCK_SIZE {
        return Err("zstd: literals section too large".into());
    }
    need(hlen + comp)?;
    let section = &block[hlen..hlen + comp];
    let mut off = 0usize;
    if lit_type == 2 {
        let (table, used) = huf_read_table(section)?;
        ctx.huf = Some(table);
        off = used;
    }
    let table =
        ctx.huf.as_ref().ok_or("zstd: treeless literals without a previous Huffman table")?;
    let streams = &section[off..];
    let mut lits = Vec::with_capacity(regen);
    if !four_streams {
        huf_decode_stream(table, streams, regen, &mut lits)?;
    } else {
        if streams.len() < 6 {
            return Err("zstd: truncated literals jump table".into());
        }
        let le16 = |i: usize| streams[i] as usize | ((streams[i + 1] as usize) << 8);
        let (s1, s2, s3) = (le16(0), le16(2), le16(4));
        let rest = streams.len() - 6;
        let s4 =
            rest.checked_sub(s1 + s2 + s3).ok_or("zstd: literals jump table exceeds section")?;
        let quarter = regen.div_ceil(4);
        let last =
            regen.checked_sub(3 * quarter).ok_or("zstd: corrupted 4-stream literals sizes")?;
        let body = &streams[6..];
        let bounds = [(0, s1), (s1, s2), (s1 + s2, s3), (s1 + s2 + s3, s4)];
        let counts = [quarter, quarter, quarter, last];
        for ((start, len), count) in bounds.into_iter().zip(counts) {
            huf_decode_stream(table, &body[start..start + len], count, &mut lits)?;
        }
    }
    Ok((lits, hlen + comp))
}

// ---- sequences section ---------------------------------------------------------------------------

#[rustfmt::skip]
const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
    16, 18, 20, 22, 24, 28, 32, 40, 48, 64, 128, 256, 512, 1024, 2048, 4096,
    8192, 16384, 32768, 65536,
];
#[rustfmt::skip]
const LL_EXTRA: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12,
    13, 14, 15, 16,
];
#[rustfmt::skip]
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
    19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
    35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027, 2051,
    4099, 8195, 16387, 32771, 65539,
];
#[rustfmt::skip]
const ML_EXTRA: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
#[rustfmt::skip]
const LL_DEFAULT: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
#[rustfmt::skip]
const ML_DEFAULT: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1,
    -1, -1, -1, -1, -1,
];
#[rustfmt::skip]
const OF_DEFAULT: [i32; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// Build (or fetch) the FSE table for one sequence-code category according to its mode.
#[allow(clippy::too_many_arguments)]
fn seq_table_for_mode(
    mode: u8,
    data: &[u8],
    pos: &mut usize,
    max_log: u32,
    max_symbol: u8,
    default_dist: &[i32],
    default_log: u32,
    prev: &mut Option<FseTable>,
) -> Result<FseTable, String> {
    match mode {
        0 => {
            let t = fse_build(default_dist, default_log)?;
            *prev = Some(t.clone());
            Ok(t)
        }
        1 => {
            let sym = *data.get(*pos).ok_or("zstd: truncated RLE sequence table")?;
            *pos += 1;
            if sym > max_symbol {
                return Err("zstd: RLE sequence symbol out of range".into());
            }
            let t = FseTable::rle(sym);
            *prev = Some(t.clone());
            Ok(t)
        }
        2 => {
            let region = &data[*pos..];
            let mut fwd = FwdBits::new(region);
            let (probs, log) = fse_read_distribution(&mut fwd, max_log, max_symbol as usize + 1)?;
            let used = fwd.bytes_consumed();
            if used > region.len() {
                return Err("zstd: truncated FSE table description".into());
            }
            *pos += used;
            let t = fse_build(&probs, log)?;
            *prev = Some(t.clone());
            Ok(t)
        }
        _ => prev
            .clone()
            .ok_or_else(|| "zstd: repeat mode without a previous sequence table".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_sequences(
    stream: &[u8],
    nseq: usize,
    ll: &FseTable,
    of: &FseTable,
    ml: &FseTable,
    literals: &[u8],
    out: &mut Vec<u8>,
    rep: &mut [usize; 3],
    block_start: usize,
) -> Result<(), String> {
    let mut rev = RevBits::new(stream)?;
    // Initial states, in literals-length / offset / match-length order.
    let mut sll = rev.read(ll.log) as usize;
    let mut sof = rev.read(of.log) as usize;
    let mut sml = rev.read(ml.log) as usize;
    if rev.overflowed() {
        return Err("zstd: truncated sequences bitstream".into());
    }
    let mut lit_pos = 0usize;
    for i in 0..nseq {
        let of_code = u32::from(of.cell(sof)?.symbol);
        let ml_code = ml.cell(sml)?.symbol as usize;
        let ll_code = ll.cell(sll)?.symbol as usize;
        if of_code > 31 || ml_code > 52 || ll_code > 35 {
            return Err("zstd: sequence code out of range".into());
        }
        // Extra bits are read offset first, then match length, then literals length.
        let of_value = (1u64 << of_code) + rev.read(of_code);
        let ml_val = ML_BASE[ml_code] as usize + rev.read(u32::from(ML_EXTRA[ml_code])) as usize;
        let ll_val = LL_BASE[ll_code] as usize + rev.read(u32::from(LL_EXTRA[ll_code])) as usize;
        if rev.overflowed() {
            return Err("zstd: truncated sequences bitstream".into());
        }
        // Resolve the offset against the three-slot repeat history.
        let offset = if of_value > 3 {
            let o = (of_value - 3) as usize;
            *rep = [o, rep[0], rep[1]];
            o
        } else {
            let idx = if ll_val == 0 { of_value as usize } else { of_value as usize - 1 };
            match idx {
                0 => rep[0],
                1 => {
                    let o = rep[1];
                    *rep = [o, rep[0], rep[2]];
                    o
                }
                2 => {
                    let o = rep[2];
                    *rep = [o, rep[0], rep[1]];
                    o
                }
                _ => {
                    let o = rep[0]
                        .checked_sub(1)
                        .filter(|&o| o != 0)
                        .ok_or("zstd: invalid repeat offset")?;
                    *rep = [o, rep[0], rep[1]];
                    o
                }
            }
        };
        if lit_pos + ll_val > literals.len() {
            return Err("zstd: sequence literals overrun".into());
        }
        out.extend_from_slice(&literals[lit_pos..lit_pos + ll_val]);
        lit_pos += ll_val;
        if offset == 0 || offset > out.len() {
            return Err("zstd: sequence offset too far back".into());
        }
        if out.len() + ml_val - block_start > MAX_BLOCK_SIZE {
            return Err("zstd: block regenerated size too large".into());
        }
        let start = out.len() - offset;
        for k in 0..ml_val {
            let b = out[start + k];
            out.push(b);
        }
        if i + 1 < nseq {
            // State update order: literals length, then match length, then offset.
            let c = ll.cell(sll)?;
            sll = c.base as usize + rev.read(u32::from(c.nbits)) as usize;
            let c = ml.cell(sml)?;
            sml = c.base as usize + rev.read(u32::from(c.nbits)) as usize;
            let c = of.cell(sof)?;
            sof = c.base as usize + rev.read(u32::from(c.nbits)) as usize;
            if rev.overflowed() {
                return Err("zstd: truncated sequences bitstream".into());
            }
        }
    }
    if !rev.finished() {
        return Err("zstd: sequences bitstream not fully consumed".into());
    }
    out.extend_from_slice(&literals[lit_pos..]);
    Ok(())
}

// ---- block / frame decoding ----------------------------------------------------------------------

/// Entropy state that persists across the blocks of one frame.
struct FrameCtx {
    huf: Option<HufTable>,
    ll: Option<FseTable>,
    of: Option<FseTable>,
    ml: Option<FseTable>,
    rep: [usize; 3],
}

fn decode_compressed_block(
    block: &[u8],
    out: &mut Vec<u8>,
    ctx: &mut FrameCtx,
) -> Result<(), String> {
    let block_start = out.len();
    let (literals, used) = decode_literals(block, ctx)?;
    let seq = &block[used..];
    let b0 = *seq.first().ok_or("zstd: missing sequences section header")? as usize;
    let (nseq, mut pos) = if b0 == 0 {
        (0, 1)
    } else if b0 < 128 {
        (b0, 1)
    } else if b0 < 255 {
        let b1 = *seq.get(1).ok_or("zstd: truncated sequences header")? as usize;
        (((b0 - 128) << 8) + b1, 2)
    } else {
        if seq.len() < 3 {
            return Err("zstd: truncated sequences header".into());
        }
        (seq[1] as usize + ((seq[2] as usize) << 8) + 0x7F00, 3)
    };
    if nseq == 0 {
        if seq.len() != pos {
            return Err("zstd: trailing bytes after empty sequences section".into());
        }
        if out.len() + literals.len() - block_start > MAX_BLOCK_SIZE {
            return Err("zstd: block regenerated size too large".into());
        }
        out.extend_from_slice(&literals);
        return Ok(());
    }
    let modes = *seq.get(pos).ok_or("zstd: missing sequence compression modes")?;
    pos += 1;
    if modes & 3 != 0 {
        return Err("zstd: reserved sequence mode bits set".into());
    }
    let ll =
        seq_table_for_mode((modes >> 6) & 3, seq, &mut pos, 9, 35, &LL_DEFAULT, 6, &mut ctx.ll)?;
    let of =
        seq_table_for_mode((modes >> 4) & 3, seq, &mut pos, 8, 31, &OF_DEFAULT, 5, &mut ctx.of)?;
    let ml =
        seq_table_for_mode((modes >> 2) & 3, seq, &mut pos, 9, 52, &ML_DEFAULT, 6, &mut ctx.ml)?;
    if pos >= seq.len() {
        return Err("zstd: missing sequences bitstream".into());
    }
    let mut rep = ctx.rep;
    execute_sequences(&seq[pos..], nseq, &ll, &of, &ml, &literals, out, &mut rep, block_start)?;
    ctx.rep = rep;
    Ok(())
}

fn decode_frame(data: &[u8], mut pos: usize, out: &mut Vec<u8>) -> Result<usize, String> {
    let frame_out_start = out.len();
    let byte = |pos: &mut usize| -> Result<u8, String> {
        let b = *data.get(*pos).ok_or("zstd: truncated frame header")?;
        *pos += 1;
        Ok(b)
    };
    let fhd = byte(&mut pos)?;
    if fhd & 0x08 != 0 {
        return Err("zstd: reserved frame header bit set".into());
    }
    let single_segment = fhd & 0x20 != 0;
    let has_checksum = fhd & 0x04 != 0;
    let did_len = [0usize, 1, 2, 4][(fhd & 3) as usize];
    let fcs_flag = fhd >> 6;
    if !single_segment {
        let wd = byte(&mut pos)?;
        let wlog = 10 + u32::from(wd >> 3);
        if wlog > 41 {
            return Err("zstd: window size too large".into());
        }
    }
    if did_len > 0 {
        let mut did = 0u64;
        for i in 0..did_len {
            did |= u64::from(byte(&mut pos)?) << (8 * i);
        }
        if did != 0 {
            return Err("zstd: dictionary frames are not supported".into());
        }
    }
    let fcs_len = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let fcs = if fcs_len > 0 {
        let mut v = 0u64;
        for i in 0..fcs_len {
            v |= u64::from(byte(&mut pos)?) << (8 * i);
        }
        if fcs_len == 2 {
            v += 256;
        }
        Some(v)
    } else {
        None
    };
    let mut ctx = FrameCtx { huf: None, ll: None, of: None, ml: None, rep: [1, 4, 8] };
    loop {
        if pos + 3 > data.len() {
            return Err("zstd: truncated block header".into());
        }
        let h =
            data[pos] as usize | ((data[pos + 1] as usize) << 8) | ((data[pos + 2] as usize) << 16);
        pos += 3;
        let last = h & 1 != 0;
        let btype = (h >> 1) & 3;
        let bsize = h >> 3;
        if bsize > MAX_BLOCK_SIZE {
            return Err("zstd: block size exceeds maximum".into());
        }
        match btype {
            0 => {
                if pos + bsize > data.len() {
                    return Err("zstd: truncated raw block".into());
                }
                out.extend_from_slice(&data[pos..pos + bsize]);
                pos += bsize;
            }
            1 => {
                let b = *data.get(pos).ok_or("zstd: truncated RLE block")?;
                pos += 1;
                out.resize(out.len() + bsize, b);
            }
            2 => {
                if pos + bsize > data.len() {
                    return Err("zstd: truncated compressed block".into());
                }
                decode_compressed_block(&data[pos..pos + bsize], out, &mut ctx)?;
                pos += bsize;
            }
            _ => return Err("zstd: reserved block type".into()),
        }
        if last {
            break;
        }
    }
    if let Some(n) = fcs {
        if (out.len() - frame_out_start) as u64 != n {
            return Err("zstd: frame content size mismatch".into());
        }
    }
    if has_checksum {
        if pos + 4 > data.len() {
            return Err("zstd: truncated content checksum".into());
        }
        let expect = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let got = xxh64(&out[frame_out_start..], 0) as u32;
        if got != expect {
            return Err("zstd: content checksum mismatch".into());
        }
    }
    Ok(pos)
}

/// Decompress one or more concatenated Zstandard frames (skippable frames are skipped).
pub fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("zstd: empty input".into());
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        if pos + 4 > data.len() {
            return Err("zstd: truncated frame".into());
        }
        let magic = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        if (0x184D_2A50..=0x184D_2A5F).contains(&magic) {
            if pos + 4 > data.len() {
                return Err("zstd: truncated skippable frame".into());
            }
            let size = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + size > data.len() {
                return Err("zstd: truncated skippable frame".into());
            }
            pos += size;
            continue;
        }
        if magic != ZSTD_MAGIC {
            return Err("zstd: bad magic number".into());
        }
        pos = decode_frame(data, pos, &mut out)?;
    }
    Ok(out)
}

// ---- encoder -------------------------------------------------------------------------------------

/// Compress into a valid single-frame Zstandard stream using raw/RLE blocks (honest framing, no
/// entropy coding), with the frame content size and an XXH64 content checksum.
pub fn zstd_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / MAX_BLOCK_SIZE * 4 + 32);
    out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    let len = data.len() as u64;
    let fcs_flag: u8 = if len < 256 {
        0
    } else if len <= 0xFFFF + 256 {
        1
    } else if len <= u64::from(u32::MAX) {
        2
    } else {
        3
    };
    // Single-segment frame (window size = content size) with a content checksum.
    out.push((fcs_flag << 6) | 0x20 | 0x04);
    match fcs_flag {
        0 => out.push(len as u8),
        1 => out.extend_from_slice(&((len - 256) as u16).to_le_bytes()),
        2 => out.extend_from_slice(&(len as u32).to_le_bytes()),
        _ => out.extend_from_slice(&len.to_le_bytes()),
    }
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00]); // last=1, Raw_Block, size 0
    } else {
        let mut chunks = data.chunks(MAX_BLOCK_SIZE).peekable();
        while let Some(chunk) = chunks.next() {
            let last = u32::from(chunks.peek().is_none());
            let uniform = chunk.len() >= 4 && chunk.iter().all(|&b| b == chunk[0]);
            let btype = u32::from(uniform); // RLE_Block when the whole chunk is one byte value
            let header = ((chunk.len() as u32) << 3) | (btype << 1) | last;
            out.extend_from_slice(&header.to_le_bytes()[..3]);
            if uniform {
                out.push(chunk[0]);
            } else {
                out.extend_from_slice(chunk);
            }
        }
    }
    out.extend_from_slice(&(xxh64(data, 0) as u32).to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // %%FIXTURES%%

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(s.len() % 2 == 0);
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn roundtrip(data: &[u8]) {
        assert_eq!(zstd_decompress(&zstd_compress(data)).unwrap(), data);
    }

    /// Parse a single-segment fixture far enough to find the first block's type (0 raw / 1 RLE /
    /// 2 compressed), so cross-oracle tests can assert the entropy-coded paths were exercised.
    fn first_block_type(frame: &[u8]) -> usize {
        assert_eq!(&frame[..4], &ZSTD_MAGIC.to_le_bytes());
        let fhd = frame[4];
        assert_eq!(fhd & 0x20, 0x20, "fixture is expected to be single-segment");
        assert_eq!(fhd & 3, 0, "fixture must not use a dictionary");
        let fcs_len = match fhd >> 6 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let h = frame[5 + fcs_len] as usize
            | ((frame[6 + fcs_len] as usize) << 8)
            | ((frame[7 + fcs_len] as usize) << 16);
        (h >> 1) & 3
    }

    fn sentence_x20() -> Vec<u8> {
        b"The quick brown fox jumps over the lazy dog. ".repeat(20)
    }

    #[test]
    fn xxh64_known_answers() {
        // Cross-checked against Bun.hash.xxHash64 (bun v1.2.21).
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        let qbf: &[u8] = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(xxh64(qbf, 0), 0x0B24_2D36_1FDA_71BC);
        assert_eq!(xxh64(qbf, 0x9E37_79B1_85EB_CA87), 0xB8A8_089A_DD7E_03D9);
        let big: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        assert_eq!(xxh64(&big, 0), 0x2527_5608_A9CF_C168);
    }

    #[test]
    fn roundtrips() {
        roundtrip(b"");
        roundtrip(b"a");
        roundtrip(b"The quick brown fox jumps over the lazy dog.");
        let all: Vec<u8> = (0..=255u8).collect();
        roundtrip(&all);
        let repetitive: Vec<u8> = (0..5000).map(|i| (i % 7) as u8).collect();
        roundtrip(&repetitive);
        roundtrip(&[0x42; 300_000]); // multi-block RLE
        // ~256 KB pseudo-random buffer: exercises the multi-block raw path.
        let mut s = 0x1234_5678u32;
        let big: Vec<u8> = (0..262_144)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s >> 24) as u8
            })
            .collect();
        roundtrip(&big);
    }

    #[test]
    fn compress_produces_expected_frame_shape() {
        let out = zstd_compress(b"hello");
        assert_eq!(&out[..4], &ZSTD_MAGIC.to_le_bytes());
        assert_eq!(out[4], 0x24); // single-segment + checksum flag, 1-byte FCS
        assert_eq!(out[5], 5); // frame content size
        let checksum = u32::from_le_bytes(out[out.len() - 4..].try_into().unwrap());
        assert_eq!(checksum, xxh64(b"hello", 0) as u32);
    }

    // Level-3 frames produced by libzstd 1.5.7. These exercise compressed sequence/literal
    // decoding independently of our raw/RLE encoder.
    #[test]
    fn reference_compressed_frames() {
        let vectors = [
            (
                "28b52ffd608402b50100d40254686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f672e200100a50a2b5506",
                sentence_x20(),
            ),
            (
                "28b52ffd6050457500003061626331323301004746f65204",
                b"abc123".repeat(3000),
            ),
            (
                "28b52ffda070110100550000107a7a01006b1139c002",
                vec![b'z'; 70_000],
            ),
        ];

        for (encoded, expected) in vectors {
            let frame = unhex(encoded);
            assert_eq!(first_block_type(&frame), 2, "expected a compressed block");
            assert_eq!(zstd_decompress(&frame).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!(zstd_decompress(&[]).is_err());
        assert!(zstd_decompress(b"not a zstd frame").is_err());
        assert!(zstd_decompress(&[0x28, 0xb5, 0x2f, 0xfd]).is_err()); // truncated
        // Dictionary frames are honestly rejected: magic, FHD with a 1-byte dictionary id,
        // window descriptor, dictionary id 1, then a (never reached) block header.
        let dict_frame = [0x28, 0xb5, 0x2f, 0xfd, 0x01, 0x00, 0x01, 0x01, 0x00, 0x00];
        assert!(zstd_decompress(&dict_frame).unwrap_err().contains("dictionar"));
    }
}
