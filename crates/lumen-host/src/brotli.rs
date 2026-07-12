//! Brotli (RFC 7932) — a from-scratch, std-only codec for `node:zlib`'s brotli* APIs.
//!
//! Decoder: complete — all meta-block types (uncompressed, metadata, compressed), simple and
//! complex prefix codes, block switching for all three categories, literal/distance context
//! modeling, and the full 122784-byte static dictionary (embedded verbatim) with all 121 word
//! transforms. Bit-exact against Node/Bun oracle output at every quality level.
//!
//! Encoder: emits *valid* streams (decodable by any conforming decoder, verified against the
//! Node oracle) using one insert-only command per meta-block with an adaptive canonical
//! Huffman code over the literals — i.e. real entropy coding but no LZ77 matching, so ratios
//! are Huffman-only (worse than gzip), far from quality 11. Falls back to uncompressed
//! meta-blocks when Huffman coding would expand the data.
//!
//! Tables (`CONTEXT_LUT`, `TRANSFORMS`, `PREFIX_SUFFIX*`) and the dictionary blob are extracted
//! from the reference implementation (brotli-1.2.0, MIT); see the tail of the file.

// ---- embedded static dictionary (RFC 7932 appendix A) ------------------------------------------

static DICTIONARY: &[u8] = include_bytes!("brotli_dictionary.bin");

/// log2 of the number of words for each word length 4..=24 (0 = no words of that length).
const DICT_SIZE_BITS: [u32; 25] = [
    0, 0, 0, 0, 10, 10, 11, 11, 10, 10, 10, 10, 10, 9, 9, 8, 7, 7, 8, 7, 7, 6, 6, 5, 5,
];
/// Byte offset of the first word of each length in `DICTIONARY`.
const DICT_OFFSETS: [u32; 25] = [
    0, 0, 0, 0, 0, 4096, 9216, 21504, 35840, 44032, 53248, 63488, 74752, 87040, 93696, 100864,
    104704, 106752, 108928, 113536, 115968, 118528, 119872, 121280, 122016,
];

// ---- bit reader (LSB-first) ---------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }
    /// Peek up to 24 bits without consuming; bits past the end of input read as 0.
    fn peek(&self, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n as usize {
            let bit_pos = self.pos + i;
            let byte = self.data.get(bit_pos >> 3).copied().unwrap_or(0);
            v |= (((byte >> (bit_pos & 7)) & 1) as u32) << i;
        }
        v
    }
    fn drop(&mut self, n: u32) {
        self.pos += n as usize;
    }
    /// Read and consume `n` bits (n <= 24), erroring past end of input.
    fn take(&mut self, n: u32) -> Result<u32, String> {
        if self.pos + n as usize > self.data.len() * 8 {
            return Err("brotli: unexpected end of input".into());
        }
        let v = self.peek(n);
        self.pos += n as usize;
        Ok(v)
    }
    /// Advance to a byte boundary; the padding bits must be zero (RFC 7932 9.1).
    fn align(&mut self) -> Result<(), String> {
        let rem = ((8 - (self.pos & 7)) & 7) as u32;
        if self.take(rem)? != 0 {
            return Err("brotli: non-zero padding bits".into());
        }
        Ok(())
    }
    fn byte_pos(&self) -> usize {
        self.pos >> 3
    }
}

// ---- canonical prefix-code decoding -------------------------------------------------------------

/// Canonical prefix-code decoder from per-symbol code lengths (max length 15), decoded bit by
/// bit — brotli writes prefix codes so that LSB-first reading walks the canonical code
/// MSB-first, the same convention as DEFLATE. `single` short-circuits zero-bit codes (1-symbol
/// simple codes and degenerate code-length codes), which consume no input.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
    single: Option<u16>,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        let mut nonzero = 0usize;
        let mut last_sym = 0u16;
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                counts[len as usize] += 1;
                nonzero += 1;
                last_sym = sym as u16;
            }
        }
        if nonzero <= 1 {
            return Huffman { counts: [0; 16], symbols: Vec::new(), single: Some(last_sym) };
        }
        let mut offsets = [0u16; 16];
        for i in 1..15 {
            offsets[i + 1] = offsets[i] + counts[i];
        }
        let mut symbols = vec![0u16; nonzero];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offsets[len as usize] as usize] = sym as u16;
                offsets[len as usize] += 1;
            }
        }
        Huffman { counts, symbols, single: None }
    }
    fn decode(&self, r: &mut BitReader) -> Result<u16, String> {
        if let Some(sym) = self.single {
            return Ok(sym);
        }
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= r.take(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("brotli: invalid prefix code".into())
    }
}

fn bit_len(mut x: u32) -> u32 {
    let mut n = 0;
    while x != 0 {
        x >>= 1;
        n += 1;
    }
    n
}

/// Order in which code-length code lengths are transmitted (RFC 7932 3.5).
const CL_ORDER: [usize; 18] = [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];
/// Static prefix code for the code-length code lengths, indexed by 4 peeked bits.
const CL_PREFIX_LEN: [u32; 16] = [2, 2, 2, 3, 2, 2, 2, 4, 2, 2, 2, 3, 2, 2, 2, 4];
const CL_PREFIX_VAL: [u8; 16] = [0, 4, 3, 2, 0, 4, 3, 1, 0, 4, 3, 2, 0, 4, 3, 5];

/// Read one prefix code (RFC 7932 3.4/3.5): simple (1-4 symbols listed literally) or complex
/// (code lengths themselves prefix-coded, with 16/17 repeat codes).
fn read_huffman_code(alphabet_size: u32, r: &mut BitReader) -> Result<Huffman, String> {
    let kind = r.take(2)?;
    if kind == 1 {
        // Simple code: NSYM in 1..=4 symbols follow, each in ALPHABET_BITS bits.
        let nsym = r.take(2)? as usize + 1;
        let max_bits = bit_len(alphabet_size - 1);
        let mut syms = [0u16; 4];
        for s in syms.iter_mut().take(nsym) {
            let v = r.take(max_bits)?;
            if v >= alphabet_size {
                return Err("brotli: simple code symbol out of range".into());
            }
            *s = v as u16;
        }
        for i in 0..nsym {
            for k in i + 1..nsym {
                if syms[i] == syms[k] {
                    return Err("brotli: duplicate simple code symbol".into());
                }
            }
        }
        let mut lengths = vec![0u8; alphabet_size as usize];
        match nsym {
            1 => {
                return Ok(Huffman { counts: [0; 16], symbols: Vec::new(), single: Some(syms[0]) })
            }
            2 => {
                lengths[syms[0] as usize] = 1;
                lengths[syms[1] as usize] = 1;
            }
            3 => {
                // The first *listed* symbol gets the 1-bit code regardless of value; the other
                // two share length 2 (assigned in canonical order, i.e. by symbol value).
                lengths[syms[0] as usize] = 1;
                lengths[syms[1] as usize] = 2;
                lengths[syms[2] as usize] = 2;
            }
            _ => {
                if r.take(1)? == 1 {
                    // tree-select: lengths 1,2,3,3
                    lengths[syms[0] as usize] = 1;
                    lengths[syms[1] as usize] = 2;
                    lengths[syms[2] as usize] = 3;
                    lengths[syms[3] as usize] = 3;
                } else {
                    for &s in &syms[..4] {
                        lengths[s as usize] = 2;
                    }
                }
            }
        }
        return Ok(Huffman::from_lengths(&lengths));
    }

    // Complex code. `kind` (0, 2, 3) is the number of leading code-length symbols skipped.
    let mut cl_lengths = [0u8; 18];
    let mut space = 32i32;
    let mut num_codes = 0;
    for i in kind as usize..18 {
        let ix = r.peek(4) as usize;
        let len = CL_PREFIX_LEN[ix];
        if r.pos + len as usize > r.data.len() * 8 {
            return Err("brotli: unexpected end of input".into());
        }
        r.drop(len);
        let v = CL_PREFIX_VAL[ix];
        cl_lengths[CL_ORDER[i]] = v;
        if v != 0 {
            space -= 32 >> v;
            num_codes += 1;
            if space <= 0 {
                break;
            }
        }
    }
    if !(num_codes == 1 || space == 0) {
        return Err("brotli: corrupt code-length code".into());
    }
    let cl = Huffman::from_lengths(&cl_lengths);

    // Decode the symbol code lengths, with 16 (repeat previous) / 17 (repeat zero) run codes.
    let mut lengths = vec![0u8; alphabet_size as usize];
    let mut symbol = 0usize;
    let mut space = 32768i64;
    let mut prev_len = 8u8; // initial repeated code length
    let mut repeat = 0u32;
    let mut repeat_len = 0u8;
    while symbol < alphabet_size as usize && space > 0 {
        let code_len = cl.decode(r)? as u8;
        if code_len < 16 {
            repeat = 0;
            if code_len != 0 {
                lengths[symbol] = code_len;
                prev_len = code_len;
                space -= 32768 >> code_len;
            }
            symbol += 1;
        } else {
            let extra = if code_len == 16 { 2 } else { 3 };
            let delta = r.take(extra)?;
            let new_len = if code_len == 16 { prev_len } else { 0 };
            if repeat_len != new_len {
                repeat = 0;
                repeat_len = new_len;
            }
            let old_repeat = repeat;
            if repeat > 0 {
                repeat = (repeat - 2) << extra;
            }
            repeat += delta + 3;
            let run = (repeat - old_repeat) as usize;
            if symbol + run > alphabet_size as usize {
                return Err("brotli: code-length run overflows alphabet".into());
            }
            if repeat_len != 0 {
                for l in &mut lengths[symbol..symbol + run] {
                    *l = repeat_len;
                }
                space -= (run as i64) << (15 - repeat_len as i64);
            }
            symbol += run;
        }
    }
    if space != 0 {
        return Err("brotli: prefix code is under/over-subscribed".into());
    }
    Ok(Huffman::from_lengths(&lengths))
}

// ---- meta-block header pieces -------------------------------------------------------------------

/// Variable-length code for values 0..=255 (RFC 7932 9.2), used for NBLTYPES/NTREES.
fn read_varlen_u8(r: &mut BitReader) -> Result<u32, String> {
    if r.take(1)? == 0 {
        return Ok(0);
    }
    let n = r.take(3)?;
    if n == 0 {
        return Ok(1);
    }
    Ok((1 << n) + r.take(n)?)
}

const BLOCK_LEN_OFFSET: [u32; 26] = [
    1, 5, 9, 13, 17, 25, 33, 41, 49, 65, 81, 97, 113, 145, 177, 209, 241, 305, 369, 497, 753,
    1265, 2289, 4337, 8433, 16625,
];
const BLOCK_LEN_NBITS: [u32; 26] = [
    2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 7, 8, 9, 10, 11, 12, 13, 24,
];

fn read_block_length(tree: &Huffman, r: &mut BitReader) -> Result<u32, String> {
    let code = tree.decode(r)? as usize;
    if code >= 26 {
        return Err("brotli: invalid block length code".into());
    }
    Ok(BLOCK_LEN_OFFSET[code] + r.take(BLOCK_LEN_NBITS[code])?)
}

fn inverse_mtf(map: &mut [u8]) {
    let mut mtf: [u8; 256] = [0; 256];
    for (i, m) in mtf.iter_mut().enumerate() {
        *m = i as u8;
    }
    for v in map.iter_mut() {
        let idx = *v as usize;
        let value = mtf[idx];
        for k in (1..=idx).rev() {
            mtf[k] = mtf[k - 1];
        }
        mtf[0] = value;
        *v = value;
    }
}

/// Context map for literals or distances (RFC 7932 7.3): RLE-of-zeros prefix code + inverse MTF.
fn read_context_map(size: usize, r: &mut BitReader) -> Result<(u32, Vec<u8>), String> {
    let num_htrees = read_varlen_u8(r)? + 1;
    let mut map = vec![0u8; size];
    if num_htrees <= 1 {
        return Ok((num_htrees, map));
    }
    let bits = r.peek(5);
    let max_run_length_prefix = if bits & 1 != 0 {
        r.drop(5);
        (bits >> 1) + 1
    } else {
        r.drop(1);
        0
    };
    let tree = read_huffman_code(num_htrees + max_run_length_prefix, r)?;
    let mut i = 0usize;
    while i < size {
        let code = tree.decode(r)? as u32;
        if code == 0 {
            map[i] = 0;
            i += 1;
        } else if code > max_run_length_prefix {
            map[i] = (code - max_run_length_prefix) as u8;
            i += 1;
        } else {
            let reps = (1usize << code) + r.take(code)? as usize;
            if i + reps > size {
                return Err("brotli: context map run overflows".into());
            }
            i += reps; // already zero-filled
        }
    }
    if r.take(1)? == 1 {
        inverse_mtf(&mut map);
    }
    Ok((num_htrees, map))
}

// ---- insert-and-copy command table (RFC 7932 5) -------------------------------------------------

const INSERT_EXTRA: [u32; 24] = [
    0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 7, 8, 9, 10, 12, 14, 24,
];
const INSERT_OFFSET: [u32; 24] = [
    0, 1, 2, 3, 4, 5, 6, 8, 10, 14, 18, 26, 34, 50, 66, 98, 130, 194, 322, 578, 1090, 2114, 6210,
    22594,
];
const COPY_EXTRA: [u32; 24] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 7, 8, 9, 10, 24,
];
const COPY_OFFSET: [u32; 24] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 18, 22, 30, 38, 54, 70, 102, 134, 198, 326, 582, 1094,
    2118,
];
/// Packed (insert hi bits | copy hi bits) for the 11 64-command cells of the alphabet.
const CELL_POS: [u32; 11] = [0, 1, 0, 1, 8, 9, 2, 16, 10, 17, 18];

struct Command {
    insert_code: usize,
    copy_code: usize,
    /// The command implies distance code 0 (reuse last distance) without reading one.
    implicit_distance: bool,
    /// Distance context id (0..=3) derived from the copy length code.
    dist_context: u8,
}

fn command_from_symbol(sym: usize) -> Command {
    let cell = sym >> 6;
    let pos = CELL_POS[cell];
    let copy_code = (((pos << 3) & 0x18) + (sym as u32 & 7)) as usize;
    let insert_code = ((pos & 0x18) + ((sym as u32 >> 3) & 7)) as usize;
    let copy_off = COPY_OFFSET[copy_code];
    Command {
        insert_code,
        copy_code,
        implicit_distance: cell < 2,
        dist_context: if copy_off > 4 { 3 } else { (copy_off - 2) as u8 },
    }
}

// ---- dictionary word transforms (RFC 7932 8) ----------------------------------------------------

/// The reference decoder's quirky UTF-8-ish uppercasing; returns the step width. Bytes that
/// fall outside `region` would be overwritten by the suffix anyway, so they are skipped.
fn to_upper_case(region: &mut [u8]) -> usize {
    if region.is_empty() {
        return 1;
    }
    if region[0] < 0xc0 {
        if region[0].is_ascii_lowercase() {
            region[0] ^= 32;
        }
        1
    } else if region[0] < 0xe0 {
        if region.len() > 1 {
            region[1] ^= 32;
        }
        2
    } else {
        if region.len() > 2 {
            region[2] ^= 5;
        }
        3
    }
}

/// Apply transform `idx` to `word`, appending the result to `out`; returns the appended length.
fn transform_word(out: &mut Vec<u8>, word: &[u8], idx: usize) -> usize {
    let prefix_id = TRANSFORMS[idx * 3] as usize;
    let kind = TRANSFORMS[idx * 3 + 1];
    let suffix_id = TRANSFORMS[idx * 3 + 2] as usize;
    let start = out.len();

    let p = PREFIX_SUFFIX_MAP[prefix_id] as usize;
    let plen = PREFIX_SUFFIX[p] as usize;
    out.extend_from_slice(&PREFIX_SUFFIX[p + 1..p + 1 + plen]);

    let mut w = word;
    match kind {
        1..=9 => w = &w[..w.len().saturating_sub(kind as usize)], // omit last 1-9
        12..=20 => w = &w[(kind as usize - 11).min(w.len())..],   // omit first 1-9
        _ => {}
    }
    let word_at = out.len();
    out.extend_from_slice(w);
    if kind == 10 {
        to_upper_case(&mut out[word_at..]);
    } else if kind == 11 {
        let mut i = word_at;
        while i < out.len() {
            let end = out.len();
            i += to_upper_case(&mut out[i..end]);
        }
    }

    let s = PREFIX_SUFFIX_MAP[suffix_id] as usize;
    let slen = PREFIX_SUFFIX[s] as usize;
    out.extend_from_slice(&PREFIX_SUFFIX[s + 1..s + 1 + slen]);
    out.len() - start
}

// ---- per-category block-switching state ---------------------------------------------------------

struct BlockState {
    num_types: u32,
    type_tree: Option<Huffman>,
    len_tree: Option<Huffman>,
    length: u32,
    /// Last two block types (for the "previous" / "second-to-last + 1" short codes).
    rb: [u32; 2],
    current: u32,
}

impl BlockState {
    fn read(r: &mut BitReader) -> Result<BlockState, String> {
        let num_types = read_varlen_u8(r)? + 1;
        let mut state = BlockState {
            num_types,
            type_tree: None,
            len_tree: None,
            length: 1 << 24,
            rb: [1, 0],
            current: 0,
        };
        if num_types >= 2 {
            state.type_tree = Some(read_huffman_code(num_types + 2, r)?);
            let len_tree = read_huffman_code(26, r)?;
            state.length = read_block_length(&len_tree, r)?;
            state.len_tree = Some(len_tree);
        }
        Ok(state)
    }
    fn switch(&mut self, r: &mut BitReader) -> Result<(), String> {
        let (Some(type_tree), Some(len_tree)) = (&self.type_tree, &self.len_tree) else {
            return Err("brotli: block length exhausted with a single block type".into());
        };
        let sym = type_tree.decode(r)? as u32;
        self.length = read_block_length(len_tree, r)?;
        let mut bt = match sym {
            0 => self.rb[0],
            1 => self.rb[1] + 1,
            _ => sym - 2,
        };
        if bt >= self.num_types {
            bt -= self.num_types;
        }
        self.rb[0] = self.rb[1];
        self.rb[1] = bt;
        self.current = bt;
        Ok(())
    }
}

// ---- decoder ------------------------------------------------------------------------------------

/// Decode a complete brotli stream.
pub fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut r = BitReader::new(data);

    // WBITS (RFC 7932 9.1).
    let window_bits = if r.take(1)? == 0 {
        16
    } else {
        let n = r.take(3)?;
        if n != 0 {
            17 + n
        } else {
            let n = r.take(3)?;
            if n == 1 {
                return Err("brotli: large-window streams are not supported".into());
            } else if n != 0 {
                8 + n
            } else {
                17
            }
        }
    };
    let max_backward = (1usize << window_bits) - 16;

    let mut out: Vec<u8> = Vec::new();
    let mut dist_rb: [i64; 4] = [16, 15, 11, 4];
    let mut dist_rb_idx: i64 = 0;

    loop {
        // --- meta-block header (RFC 7932 9.2) ---
        let is_last = r.take(1)? == 1;
        if is_last && r.take(1)? == 1 {
            break; // ISLASTEMPTY
        }
        let nibbles = r.take(2)? + 4;
        let mut mlen: usize;
        if nibbles == 7 {
            // Metadata meta-block: byte-aligned payload to skip.
            if r.take(1)? != 0 {
                return Err("brotli: reserved bit set".into());
            }
            let skip_bytes = r.take(2)?;
            mlen = 0;
            for i in 0..skip_bytes {
                let b = r.take(8)? as usize;
                if i + 1 == skip_bytes && skip_bytes > 1 && b == 0 {
                    return Err("brotli: exuberant metadata size byte".into());
                }
                mlen |= b << (8 * i);
            }
            if skip_bytes > 0 {
                mlen += 1;
            }
            r.align()?;
            if r.byte_pos() + mlen > data.len() {
                return Err("brotli: metadata overruns input".into());
            }
            r.drop((mlen * 8) as u32);
            if is_last {
                break;
            }
            continue;
        }
        mlen = 0;
        for i in 0..nibbles {
            let v = r.take(4)? as usize;
            if i + 1 == nibbles && nibbles > 4 && v == 0 {
                return Err("brotli: exuberant size nibble".into());
            }
            mlen |= v << (4 * i);
        }
        mlen += 1;
        if !is_last && r.take(1)? == 1 {
            // Uncompressed meta-block: byte-aligned raw copy.
            r.align()?;
            let start = r.byte_pos();
            if start + mlen > data.len() {
                return Err("brotli: uncompressed block overruns input".into());
            }
            out.extend_from_slice(&data[start..start + mlen]);
            r.drop((mlen * 8) as u32);
            continue;
        }

        // --- compressed meta-block header ---
        let mut lit_block = BlockState::read(&mut r)?;
        let mut cmd_block = BlockState::read(&mut r)?;
        let mut dist_block = BlockState::read(&mut r)?;

        let bits = r.take(6)?;
        let npostfix = bits & 3;
        let ndirect = (bits >> 2) << npostfix;

        let mut context_modes = Vec::with_capacity(lit_block.num_types as usize);
        for _ in 0..lit_block.num_types {
            context_modes.push(r.take(2)? as u8);
        }

        let (num_lit_htrees, lit_context_map) =
            read_context_map((lit_block.num_types as usize) << 6, &mut r)?;
        let (num_dist_htrees, dist_context_map) =
            read_context_map((dist_block.num_types as usize) << 2, &mut r)?;

        let mut lit_trees = Vec::with_capacity(num_lit_htrees as usize);
        for _ in 0..num_lit_htrees {
            lit_trees.push(read_huffman_code(256, &mut r)?);
        }
        let mut cmd_trees = Vec::with_capacity(cmd_block.num_types as usize);
        for _ in 0..cmd_block.num_types {
            cmd_trees.push(read_huffman_code(704, &mut r)?);
        }
        let dist_alphabet = 16 + ndirect + (24 << (npostfix + 1));
        let mut dist_trees = Vec::with_capacity(num_dist_htrees as usize);
        for _ in 0..num_dist_htrees {
            dist_trees.push(read_huffman_code(dist_alphabet, &mut r)?);
        }

        // Distance code -> (extra bits, offset) LUT (RFC 7932 4).
        let mut dist_extra = vec![0u32; dist_alphabet as usize];
        let mut dist_offset = vec![0u32; dist_alphabet as usize];
        {
            let postfix = 1u32 << npostfix;
            let mut i = 16usize;
            for j in 0..ndirect {
                dist_offset[i] = j + 1;
                i += 1;
            }
            let mut nbits = 1u32;
            let mut half = 0u32;
            while i < dist_alphabet as usize {
                let base = ndirect + ((((2 + half) << nbits) - 4) << npostfix) + 1;
                for j in 0..postfix {
                    dist_extra[i] = nbits;
                    dist_offset[i] = base + j;
                    i += 1;
                }
                nbits += half;
                half ^= 1;
            }
        }

        // --- meta-block body ---
        let mut remaining = mlen as i64;
        while remaining > 0 {
            if cmd_block.length == 0 {
                cmd_block.switch(&mut r)?;
            }
            cmd_block.length -= 1;
            let cmd_sym = cmd_trees[cmd_block.current as usize].decode(&mut r)? as usize;
            let cmd = command_from_symbol(cmd_sym);
            let insert_len = INSERT_OFFSET[cmd.insert_code] as usize
                + r.take(INSERT_EXTRA[cmd.insert_code])? as usize;
            let copy_len =
                COPY_OFFSET[cmd.copy_code] as usize + r.take(COPY_EXTRA[cmd.copy_code])? as usize;

            // Literals.
            for _ in 0..insert_len {
                if lit_block.length == 0 {
                    lit_block.switch(&mut r)?;
                }
                lit_block.length -= 1;
                let p1 = if out.is_empty() { 0 } else { out[out.len() - 1] } as usize;
                let p2 = if out.len() < 2 { 0 } else { out[out.len() - 2] } as usize;
                let mode = context_modes[lit_block.current as usize] as usize;
                let lut = &CONTEXT_LUT[mode << 9..];
                let ctx = (lut[p1] | lut[256 + p2]) as usize;
                let tree_idx = lit_context_map[((lit_block.current as usize) << 6) + ctx] as usize;
                let lit = lit_trees[tree_idx].decode(&mut r)? as u8;
                out.push(lit);
            }
            remaining -= insert_len as i64;
            if remaining <= 0 {
                break;
            }

            // Distance: implicit (reuse last), short ring-buffer code, or coded.
            let mut rb_peeked = 0i64; // 1 when the ring-buffer index was rolled back by a peek
            let distance: i64;
            if cmd.implicit_distance {
                distance = dist_rb[((dist_rb_idx - 1) & 3) as usize];
                dist_rb_idx -= 1;
                rb_peeked = 1;
            } else {
                if dist_block.length == 0 {
                    dist_block.switch(&mut r)?;
                }
                dist_block.length -= 1;
                let tree_idx = dist_context_map
                    [((dist_block.current as usize) << 2) + cmd.dist_context as usize]
                    as usize;
                let sym = dist_trees[tree_idx].decode(&mut r)? as usize;
                if sym < 16 {
                    // Short codes reference the distance ring buffer (RFC 7932 4).
                    if sym == 0 {
                        distance = dist_rb[((dist_rb_idx - 1) & 3) as usize];
                        dist_rb_idx -= 1;
                        rb_peeked = 1;
                    } else if sym < 4 {
                        distance = dist_rb[((dist_rb_idx - 1 - sym as i64) & 3) as usize];
                    } else {
                        let (slot, base) = if sym < 10 { (1i64, sym - 4) } else { (2i64, sym - 10) };
                        let delta = (((0x0060_5142u32 >> (4 * base)) & 0xf) as i64) - 3;
                        distance = dist_rb[((dist_rb_idx - slot) & 3) as usize] + delta;
                        if distance <= 0 {
                            return Err("brotli: invalid short distance".into());
                        }
                    }
                } else {
                    let extra = r.take(dist_extra[sym])? as i64;
                    distance = dist_offset[sym] as i64 + (extra << npostfix);
                }
            }

            let max_distance = out.len().min(max_backward) as i64;
            if distance > max_distance {
                // Static dictionary reference.
                if !(4..=24).contains(&copy_len) || DICT_SIZE_BITS[copy_len] == 0 {
                    return Err("brotli: invalid dictionary reference length".into());
                }
                let address = (distance - max_distance - 1) as usize;
                let shift = DICT_SIZE_BITS[copy_len];
                let word_idx = address & ((1 << shift) - 1);
                let transform_idx = address >> shift;
                if transform_idx >= NUM_TRANSFORMS {
                    return Err("brotli: invalid dictionary transform".into());
                }
                let offset = DICT_OFFSETS[copy_len] as usize + word_idx * copy_len;
                let word = &DICTIONARY[offset..offset + copy_len];
                let written = transform_word(&mut out, word, transform_idx);
                if written == 0 && distance <= 120 {
                    return Err("brotli: empty transformed dictionary word".into());
                }
                remaining -= written as i64;
                dist_rb_idx += rb_peeked; // dictionary hits don't consume the peeked slot
            } else {
                if distance <= 0 {
                    return Err("brotli: invalid distance".into());
                }
                dist_rb[(dist_rb_idx & 3) as usize] = distance;
                dist_rb_idx += 1;
                let start = out.len() - distance as usize;
                for k in 0..copy_len {
                    let b = out[start + k];
                    out.push(b);
                }
                remaining -= copy_len as i64;
            }
        }
        if remaining < 0 {
            return Err("brotli: meta-block length mismatch".into());
        }
        if is_last {
            break;
        }
    }
    Ok(out)
}

// ---- encoder ------------------------------------------------------------------------------------

struct BitWriter {
    out: Vec<u8>,
    bit_buf: u64,
    bit_cnt: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter { out: Vec::new(), bit_buf: 0, bit_cnt: 0 }
    }
    fn write(&mut self, value: u32, n: u32) {
        self.bit_buf |= (value as u64) << self.bit_cnt;
        self.bit_cnt += n;
        while self.bit_cnt >= 8 {
            self.out.push((self.bit_buf & 0xff) as u8);
            self.bit_buf >>= 8;
            self.bit_cnt -= 8;
        }
    }
    /// Prefix codes are written MSB-first (bit-reversed relative to the LSB bit order).
    fn write_code(&mut self, code: u32, n: u32) {
        let mut reversed = 0;
        for i in 0..n {
            reversed |= ((code >> i) & 1) << (n - 1 - i);
        }
        self.write(reversed, n);
    }
    fn align(&mut self) {
        if self.bit_cnt > 0 {
            self.out.push((self.bit_buf & 0xff) as u8);
            self.bit_buf = 0;
            self.bit_cnt = 0;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.bit_cnt > 0 {
            self.out.push((self.bit_buf & 0xff) as u8);
        }
        self.out
    }
}

/// Huffman code lengths for `freqs` capped at `max_len`, via count-scaling retries (halve the
/// counts until the unrestricted Huffman depth fits — near-optimal and always a complete code).
fn huffman_lengths(freqs: &[u64], max_len: u32) -> Vec<u8> {
    let mut scaled: Vec<u64> = freqs.to_vec();
    loop {
        let lengths = plain_huffman_lengths(&scaled);
        if lengths.iter().all(|&l| (l as u32) <= max_len) {
            return lengths;
        }
        for f in scaled.iter_mut() {
            if *f > 0 {
                *f = (*f + 1) >> 1;
            }
        }
    }
}

/// Unrestricted Huffman code lengths by pairing the two lightest subtrees (O(n^2), n <= 256).
fn plain_huffman_lengths(freqs: &[u64]) -> Vec<u8> {
    let mut lengths = vec![0u8; freqs.len()];
    let mut nodes: Vec<(u64, Vec<usize>)> =
        freqs.iter().enumerate().filter(|&(_, &f)| f > 0).map(|(i, &f)| (f, vec![i])).collect();
    if nodes.len() == 1 {
        lengths[nodes[0].1[0]] = 1;
        return lengths;
    }
    while nodes.len() > 1 {
        nodes.sort_by(|a, b| b.0.cmp(&a.0)); // descending; the two lightest sit at the tail
        let (wa, mut ga) = nodes.pop().expect("len > 1");
        let (wb, gb) = nodes.pop().expect("len > 1");
        for &s in ga.iter().chain(gb.iter()) {
            lengths[s] += 1;
        }
        ga.extend(gb);
        nodes.push((wa + wb, ga));
    }
    lengths
}

/// Canonical code values for per-symbol lengths (ties broken by symbol order, matching the
/// decoder's table construction).
fn canonical_codes(lengths: &[u8]) -> Vec<u32> {
    let mut counts = [0u32; 16];
    for &l in lengths {
        counts[l as usize] += 1;
    }
    counts[0] = 0;
    let mut next = [0u32; 16];
    let mut code = 0u32;
    for len in 1..16 {
        code = (code + counts[len - 1]) << 1;
        next[len] = code;
    }
    lengths
        .iter()
        .map(|&l| {
            if l == 0 {
                0
            } else {
                let c = next[l as usize];
                next[l as usize] += 1;
                c
            }
        })
        .collect()
}

/// Static code for code-length code lengths: value -> (bits written LSB-first, bit count).
const CL_STATIC: [(u32, u32); 6] = [(0, 2), (7, 4), (3, 3), (2, 2), (1, 2), (15, 4)];

/// Emit a complex prefix-code header for `lengths` (no 16/17 run codes — plain but valid).
fn write_complex_code(w: &mut BitWriter, lengths: &[u8]) {
    w.write(0, 2); // HSKIP = 0

    let last_nonzero = lengths.iter().rposition(|&l| l != 0).expect("nonempty code") + 1;
    let mut used: Vec<u8> = Vec::new();
    for &l in &lengths[..last_nonzero] {
        if !used.contains(&l) {
            used.push(l);
        }
    }

    if used.len() == 1 {
        // One distinct length v and no gaps: symbols 0..2^v all have length v (a complete
        // code), which is exactly the decoder's degenerate "single code-length code" fill
        // behavior (it assigns length v to symbols from 0 until the code-space fills, reading
        // no symbol-length bits). Emit all 18 wire entries — the code-length space check
        // passes via its num_codes == 1 escape hatch — and no symbol lengths at all.
        let v = used[0] as usize;
        debug_assert_eq!(last_nonzero, 1usize << v, "uniform code must be complete");
        for &sym in CL_ORDER.iter() {
            // Give the single used length value a 1-bit code-length code; everything else 0.
            let (bits, n) = if sym == v { CL_STATIC[1] } else { CL_STATIC[0] };
            w.write(bits, n);
        }
        return;
    }

    // Balanced complete code-length code over the distinct length values used (k >= 2):
    // ceil(log2 k) deep, with (2^depth - k) values promoted one level, most frequent first.
    let k = used.len() as u32;
    let depth = bit_len(k - 1).max(1);
    let num_short = (1u32 << depth) - k;
    let mut by_freq: Vec<(usize, u8)> = used
        .iter()
        .map(|&l| (lengths[..last_nonzero].iter().filter(|&&x| x == l).count(), l))
        .collect();
    by_freq.sort_by(|a, b| b.0.cmp(&a.0));
    let mut cl_len = [0u8; 18];
    for (i, &(_, l)) in by_freq.iter().enumerate() {
        cl_len[l as usize] = if (i as u32) < num_short { (depth - 1) as u8 } else { depth as u8 };
    }

    // Emit the code-length code lengths in wire order, stopping once the code is complete.
    let mut space = 32i32;
    for &sym in CL_ORDER.iter() {
        let v = cl_len[sym] as usize;
        let (bits, n) = CL_STATIC[v];
        w.write(bits, n);
        if v != 0 {
            space -= 32 >> v;
            if space <= 0 {
                break;
            }
        }
    }

    // Emit the symbol lengths through the code-length code.
    let cl_codes = canonical_codes(&cl_len);
    for &l in &lengths[..last_nonzero] {
        w.write_code(cl_codes[l as usize], cl_len[l as usize] as u32);
    }
}

/// Emit the header + body of one compressed meta-block: a single insert-only command whose
/// insert length covers the whole chunk, with Huffman-coded literals.
fn write_literal_metablock(w: &mut BitWriter, chunk: &[u8], is_last: bool) {
    let mlen = chunk.len();
    w.write(u32::from(is_last), 1); // ISLAST
    if is_last {
        w.write(0, 1); // ISLASTEMPTY = 0
    }
    let nibbles: u32 = if mlen - 1 < 1 << 16 {
        4
    } else if mlen - 1 < 1 << 20 {
        5
    } else {
        6
    };
    w.write(nibbles - 4, 2);
    w.write((mlen - 1) as u32, nibbles * 4);
    if !is_last {
        w.write(0, 1); // ISUNCOMPRESSED = 0
    }

    w.write(0, 1); // NBLTYPESL = 1
    w.write(0, 1); // NBLTYPESI = 1
    w.write(0, 1); // NBLTYPESD = 1
    w.write(0, 6); // NPOSTFIX = 0, NDIRECT = 0
    w.write(0, 2); // literal context mode 0 (irrelevant with a single literal tree)
    w.write(0, 1); // NTREESL = 1
    w.write(0, 1); // NTREESD = 1

    // Literal code: simple when <= 4 distinct byte values, else an adaptive complex code.
    let mut freqs = [0u64; 256];
    for &b in chunk {
        freqs[b as usize] += 1;
    }
    let distinct: Vec<usize> = (0..256).filter(|&i| freqs[i] > 0).collect();
    let mut lengths = vec![0u8; 256];
    if distinct.len() <= 4 {
        // Most-frequent-first: the first listed symbol gets the shortest code in the skewed
        // simple-code shapes.
        let mut order = distinct.clone();
        order.sort_by(|&a, &b| freqs[b].cmp(&freqs[a]));
        w.write(1, 2); // simple code
        w.write(order.len() as u32 - 1, 2);
        for &s in &order {
            w.write(s as u32, 8); // ALPHABET_BITS(256) = 8
        }
        match order.len() {
            1 => {}
            2 => {
                lengths[order[0]] = 1;
                lengths[order[1]] = 1;
            }
            3 => {
                lengths[order[0]] = 1;
                lengths[order[1]] = 2;
                lengths[order[2]] = 2;
            }
            _ => {
                w.write(1, 1); // tree-select: lengths 1,2,3,3
                lengths[order[0]] = 1;
                lengths[order[1]] = 2;
                lengths[order[2]] = 3;
                lengths[order[3]] = 3;
            }
        }
    } else {
        lengths = huffman_lengths(&freqs, 15);
        write_complex_code(w, &lengths);
    }
    let codes = canonical_codes(&lengths);

    // Command code: one symbol whose insert code covers mlen (copy part never used because the
    // insert exhausts the meta-block).
    let ins_code = (0..24)
        .rev()
        .find(|&c| INSERT_OFFSET[c] as usize <= mlen)
        .expect("mlen >= 1 fits an insert code");
    let cmd_sym = [128u32, 256, 448][ins_code >> 3] + (((ins_code & 7) as u32) << 3);
    w.write(1, 2); // simple code
    w.write(0, 2); // NSYM = 1
    w.write(cmd_sym, 10); // ALPHABET_BITS(704) = 10

    // Distance code: one dummy symbol, never referenced.
    w.write(1, 2);
    w.write(0, 2);
    w.write(0, 6); // ALPHABET_BITS(16 + 0 + 48) = 6

    // Body: the command costs 0 bits; its insert extra bits, then the literals (the single
    // copy-length code 2 has no extra bits).
    w.write((mlen - INSERT_OFFSET[ins_code] as usize) as u32, INSERT_EXTRA[ins_code]);
    for &b in chunk {
        w.write_code(codes[b as usize], lengths[b as usize] as u32);
    }
}

fn write_uncompressed_metablock(w: &mut BitWriter, chunk: &[u8]) {
    let mlen = chunk.len();
    w.write(0, 1); // ISLAST = 0 (ISUNCOMPRESSED exists only on non-last blocks)
    let nibbles: u32 = if mlen - 1 < 1 << 16 {
        4
    } else if mlen - 1 < 1 << 20 {
        5
    } else {
        6
    };
    w.write(nibbles - 4, 2);
    w.write((mlen - 1) as u32, nibbles * 4);
    w.write(1, 1); // ISUNCOMPRESSED
    w.align();
    w.out.extend_from_slice(chunk);
}

/// Encode `data` as a valid brotli stream (see the module docs for the honest ratio caveat).
pub fn brotli_compress(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    // WBITS = 22 (Node's default lgwin), encoded as 1 followed by (22 - 17) in 3 bits.
    w.write(1, 1);
    w.write(5, 3);

    if data.is_empty() {
        w.write(1, 1); // ISLAST
        w.write(1, 1); // ISLASTEMPTY
        return w.finish();
    }

    const MAX_MLEN: usize = 1 << 24;
    let chunks: Vec<&[u8]> = data.chunks(MAX_MLEN).collect();
    let mut needs_final_empty = false;
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last_chunk = i + 1 == chunks.len();
        // Estimate the Huffman-coded size; fall back to an uncompressed meta-block on expansion.
        let mut freqs = [0u64; 256];
        for &b in chunk.iter() {
            freqs[b as usize] += 1;
        }
        let distinct = freqs.iter().filter(|&&f| f > 0).count();
        let est_bits: u64 = if distinct <= 4 {
            chunk.len() as u64 * 3 + 80
        } else {
            let lengths = huffman_lengths(&freqs, 15);
            (0..256).map(|s| freqs[s] * lengths[s] as u64).sum::<u64>() + 256 * 5 + 120
        };
        if est_bits / 8 < chunk.len() as u64 {
            write_literal_metablock(&mut w, chunk, is_last_chunk);
            needs_final_empty = false;
        } else {
            write_uncompressed_metablock(&mut w, chunk);
            needs_final_empty = true;
        }
    }
    if needs_final_empty {
        w.write(1, 1); // ISLAST
        w.write(1, 1); // ISLASTEMPTY
    }
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::{brotli_compress, brotli_decompress};

    fn unhex(s: &str) -> Vec<u8> {
        s.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |b: u8| match b {
                    b'0'..=b'9' => b - b'0',
                    b'a'..=b'f' => b - b'a' + 10,
                    _ => panic!("invalid hex fixture"),
                };
                digit(pair[0]) << 4 | digit(pair[1])
            })
            .collect()
    }

    #[test]
    fn decodes_reference_vectors() {
        let vectors = [
            ("06", Vec::new()),
            ("0b028068656c6c6f03", b"hello".to_vec()),
            (
                "1b2a0000c4dc46a95e0d0b45712af29c4cfe1c4517a82a5b4956982fbcf174445e94d5c604b7da8131d5fd87c004",
                b"The quick brown fox jumps over the lazy dog".to_vec(),
            ),
            (
                "1b6f17000476c0e62e73216e8b03c90340770c",
                b"abc123".repeat(1000),
            ),
        ];

        for (encoded, expected) in vectors {
            assert_eq!(brotli_decompress(&unhex(encoded)).unwrap(), expected);
        }
    }

    #[test]
    fn roundtrips_representative_inputs() {
        let binary: Vec<u8> = (0..=255).collect();
        for input in [
            Vec::new(),
            b"hello".to_vec(),
            b"abc123".repeat(1000),
            binary,
        ] {
            let encoded = brotli_compress(&input);
            assert_eq!(brotli_decompress(&encoded).unwrap(), input);
        }
    }
}

// ---- tables extracted from the reference implementation (brotli-1.2.0, MIT) --------------------

static PREFIX_SUFFIX: [u8; 217] = [1,32,2,44,32,8,32,111,102,32,116,104,101,32,4,32,111,102,32,2,115,32,1,46,5,32,97,110,100,32,4,32,105,110,32,1,34,4,32,116,111,32,2,34,62,1,10,2,46,32,1,93,5,32,102,111,114,32,3,32,97,32,6,32,116,104,97,116,32,1,39,6,32,119,105,116,104,32,6,32,102,114,111,109,32,4,32,98,121,32,1,40,6,46,32,84,104,101,32,4,32,111,110,32,4,32,97,115,32,4,32,105,115,32,4,105,110,103,32,2,10,9,1,58,3,101,100,32,2,61,34,4,32,97,116,32,3,108,121,32,1,44,2,61,39,5,46,99,111,109,47,7,46,32,84,104,105,115,32,5,32,110,111,116,32,3,101,114,32,3,97,108,32,4,102,117,108,32,4,105,118,101,32,5,108,101,115,115,32,4,101,115,116,32,4,105,122,101,32,2,194,160,4,111,117,115,32,5,32,116,104,101,32,2,101,32,0,];
static PREFIX_SUFFIX_MAP: [u16; 50] = [0,2,5,14,19,22,24,30,35,37,42,45,47,50,52,58,62,69,71,78,85,90,92,99,104,109,114,119,122,124,128,131,136,140,142,145,151,159,165,169,173,178,183,189,194,199,202,207,213,216,];
const NUM_TRANSFORMS: usize = 121;
static TRANSFORMS: [u8; 363] = [49,0,49,49,0,0,0,0,0,49,12,49,49,10,0,49,0,47,0,0,49,4,0,0,49,0,3,49,10,49,49,0,6,49,13,49,49,1,49,1,0,0,49,0,1,0,10,0,49,0,7,49,0,9,48,0,0,49,0,8,49,0,5,49,0,10,49,0,11,49,3,49,49,0,13,49,0,14,49,14,49,49,2,49,49,0,15,49,0,16,0,10,49,49,0,12,5,0,49,0,0,1,49,15,49,49,0,18,49,0,17,49,0,19,49,0,20,49,16,49,49,17,49,47,0,49,49,4,49,49,0,22,49,11,49,49,0,23,49,0,24,49,0,25,49,7,49,49,1,26,49,0,27,49,0,28,0,0,12,49,0,29,49,20,49,49,18,49,49,6,49,49,0,21,49,10,1,49,8,49,49,0,31,49,0,32,47,0,3,49,5,49,49,9,49,0,10,1,49,10,8,5,0,21,49,11,0,49,10,10,49,0,30,0,0,5,35,0,49,47,0,2,49,10,17,49,0,36,49,0,33,5,0,0,49,10,21,49,10,5,49,0,37,0,0,30,49,0,38,0,11,0,49,0,39,0,11,49,49,0,34,49,11,8,49,10,12,0,0,21,49,0,40,0,10,12,49,0,41,49,0,42,49,11,17,49,0,43,0,10,5,49,11,10,0,0,34,49,10,33,49,0,44,49,11,5,45,0,49,0,0,33,49,10,30,49,11,30,49,0,46,49,11,1,49,10,34,0,10,33,0,11,30,0,11,1,49,11,33,49,11,21,49,11,12,0,11,5,49,11,34,0,11,12,0,10,30,0,11,34,0,10,34,];
static CONTEXT_LUT: [u8; 2048] = [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,6,6,6,6,7,7,7,7,8,8,8,8,9,9,9,9,10,10,10,10,11,11,11,11,12,12,12,12,13,13,13,13,14,14,14,14,15,15,15,15,16,16,16,16,17,17,17,17,18,18,18,18,19,19,19,19,20,20,20,20,21,21,21,21,22,22,22,22,23,23,23,23,24,24,24,24,25,25,25,25,26,26,26,26,27,27,27,27,28,28,28,28,29,29,29,29,30,30,30,30,31,31,31,31,32,32,32,32,33,33,33,33,34,34,34,34,35,35,35,35,36,36,36,36,37,37,37,37,38,38,38,38,39,39,39,39,40,40,40,40,41,41,41,41,42,42,42,42,43,43,43,43,44,44,44,44,45,45,45,45,46,46,46,46,47,47,47,47,48,48,48,48,49,49,49,49,50,50,50,50,51,51,51,51,52,52,52,52,53,53,53,53,54,54,54,54,55,55,55,55,56,56,56,56,57,57,57,57,58,58,58,58,59,59,59,59,60,60,60,60,61,61,61,61,62,62,62,62,63,63,63,63,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,4,4,0,0,4,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,8,12,16,12,12,20,12,16,24,28,12,12,32,12,36,12,44,44,44,44,44,44,44,44,44,44,32,32,24,40,28,12,12,48,52,52,52,48,52,52,52,48,52,52,52,52,52,48,52,52,52,52,52,48,52,52,52,52,52,24,12,28,12,12,12,56,60,60,60,56,60,60,60,56,60,60,60,60,60,56,60,60,60,60,60,56,60,60,60,60,60,24,12,28,12,0,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,0,1,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,2,3,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,2,2,2,2,2,2,2,2,2,2,1,1,1,1,1,1,1,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,1,1,1,1,1,1,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,1,1,1,1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,0,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,16,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,24,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,32,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,40,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,56,0,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,5,6,6,6,6,6,6,6,6,6,6,6,6,6,6,6,7,];
