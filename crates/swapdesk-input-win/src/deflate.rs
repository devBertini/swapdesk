//! A small zlib (RFC 1950) and deflate (RFC 1951) compressor for the PNG a `CF_DIB` becomes (review of 22/11/2026,
//! D8): stored blocks made a 4K screenshot about 24 MiB, past the 16 MiB clipboard limit of PROTOCOL.md 7.5, and the
//! workspace has no compression crate (a new one needs the owner's yes).
//!
//! LZ77 over the 32 KiB window with hash chains, greedy matching; each block of up to [`BLOCK_TOKENS`] tokens is
//! written with whichever of the fixed codes, its own (dynamic) Huffman codes or stored bytes is the shortest, so the
//! output is never much longer than the input. Code lengths are limited to 15 bits (7 for the code-length code) by
//! the Kraft-sum adjustment miniz and zlib's `trees.c` use.

/// The window of deflate.
const WINDOW: usize = 32 * 1024;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
/// Candidates looked at per position: enough for screen content (long runs, repeated rows) at a few hundred MB/s.
const MAX_CHAIN: usize = 16;
/// A match this long is taken without looking further.
const NICE_MATCH: usize = 128;
/// Tokens per block: each block gets codes fitted to its own content.
const BLOCK_TOKENS: usize = 1 << 16;

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// The order the code-length code's lengths are written in.
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
const END_OF_BLOCK: usize = 256;

/// Adler-32 of `data` (the zlib trailer), summed in runs of 5552 bytes so the modulo is taken once per run.
pub(crate) fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for run in data.chunks(5552) {
        for &x in run {
            a += u32::from(x);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// `data` as a zlib stream.
pub(crate) fn zlib(data: &[u8]) -> Vec<u8> {
    let mut bits = Bits { out: Vec::with_capacity(data.len() / 4 + 64), acc: 0, n: 0 };
    // CMF 0x78 (deflate, 32 KiB window), FLG 0x01 (fastest level; 0x7801 is a multiple of 31).
    bits.out.extend_from_slice(&[0x78, 0x01]);
    deflate(data, &mut bits);
    let mut out = bits.finish();
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[derive(Clone, Copy)]
enum Token {
    Literal(u8),
    Match { len: u16, dist: u16 },
}

/// Bits written least significant first, as deflate packs them.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= u64::from(value) << self.n;
        self.n += count;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// A Huffman code, which deflate writes most significant bit first.
    fn code(&mut self, code: u16, len: u8) {
        if len > 0 {
            self.put(u32::from(code.reverse_bits() >> (16 - len)), u32::from(len));
        }
    }

    fn align(&mut self) {
        if self.n > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

fn len_index(len: usize) -> usize {
    if len == MAX_MATCH { 28 } else { LEN_BASE.partition_point(|&b| usize::from(b) <= len) - 1 }
}

fn dist_index(dist: usize) -> usize {
    DIST_BASE.partition_point(|&b| usize::from(b) <= dist) - 1
}

fn hash(data: &[u8], i: usize) -> usize {
    let v = u32::from(data[i]) << 16 | u32::from(data[i + 1]) << 8 | u32::from(data[i + 2]);
    (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

fn deflate(data: &[u8], bits: &mut Bits) {
    let n = data.len();
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let mut prev = vec![u32::MAX; WINDOW];
    let mut tokens = Vec::with_capacity(BLOCK_TOKENS);
    let (mut block_start, mut i) = (0usize, 0usize);
    let insert = |head: &mut [u32], prev: &mut [u32], at: usize| {
        let h = hash(data, at);
        prev[at % WINDOW] = head[h];
        head[h] = at as u32;
    };
    while i < n {
        let (mut best_len, mut best_dist) = (0usize, 0usize);
        if i + MIN_MATCH <= n {
            let max_len = (n - i).min(MAX_MATCH);
            let mut candidate = head[hash(data, i)];
            let mut chain = MAX_CHAIN;
            // Positions in the chain decrease; the slot of one within the window was last written by that position.
            while candidate != u32::MAX && chain > 0 {
                let c = candidate as usize;
                if i - c > WINDOW {
                    break;
                }
                if data[c + best_len] == data[i + best_len] {
                    let len =
                        data[c..c + max_len].iter().zip(&data[i..i + max_len]).take_while(|(a, b)| a == b).count();
                    if len > best_len {
                        (best_len, best_dist) = (len, i - c);
                        if len >= NICE_MATCH.min(max_len) {
                            break;
                        }
                    }
                }
                candidate = prev[c % WINDOW];
                chain -= 1;
            }
            insert(&mut head, &mut prev, i);
        }
        if best_len >= MIN_MATCH {
            tokens.push(Token::Match { len: best_len as u16, dist: best_dist as u16 });
            for at in i + 1..i + best_len {
                if at + MIN_MATCH <= n {
                    insert(&mut head, &mut prev, at);
                }
            }
            i += best_len;
        } else {
            tokens.push(Token::Literal(data[i]));
            i += 1;
        }
        if tokens.len() == BLOCK_TOKENS && i < n {
            block(bits, &tokens, &data[block_start..i], false);
            tokens.clear();
            block_start = i;
        }
    }
    block(bits, &tokens, &data[block_start..], true);
}

/// The Huffman code lengths for `freq`, none longer than `limit`; unused symbols get 0. A lone used symbol gets a
/// one-bit code beside a second one: zlib refuses an incomplete code-length code.
fn code_lengths(freq: &[u32], limit: u8) -> Vec<u8> {
    let mut lens = vec![0u8; freq.len()];
    let mut used: Vec<usize> = (0..freq.len()).filter(|&s| freq[s] > 0).collect();
    match used.len() {
        0 => return lens,
        1 => {
            lens[used[0]] = 1;
            lens[usize::from(used[0] == 0)] = 1;
            return lens;
        }
        _ => {}
    }
    // Huffman's construction over the used symbols, rarest first; ties broken by symbol so the result is stable.
    used.sort_by_key(|&s| (freq[s], s));
    let leaves = used.len();
    let mut weight: Vec<u64> = used.iter().map(|&s| u64::from(freq[s])).collect();
    let mut parent = vec![usize::MAX; 2 * leaves - 1];
    let (mut leaf, mut inner) = (0usize, leaves);
    let take = |weight: &[u64], leaf: &mut usize, inner: &mut usize, next: usize| {
        // The two queues (leaves in order, inner nodes in order of creation) are each sorted by weight.
        if *leaf < leaves && (*inner >= next || weight[*leaf] <= weight[*inner]) {
            *leaf += 1;
            *leaf - 1
        } else {
            *inner += 1;
            *inner - 1
        }
    };
    for next in leaves..2 * leaves - 1 {
        let a = take(&weight, &mut leaf, &mut inner, next);
        let b = take(&weight, &mut leaf, &mut inner, next);
        weight.push(weight[a] + weight[b]);
        parent[a] = next;
        parent[b] = next;
    }
    // Depth of each node from the root (the last one made), then of each leaf.
    let mut depth = vec![0u32; 2 * leaves - 1];
    for node in (0..2 * leaves - 2).rev() {
        depth[node] = depth[parent[node]] + 1;
    }
    // Lengths over the limit: count codes per length, fold the long ones into the limit, then lengthen short ones
    // until the Kraft sum is exactly one again.
    let limit = usize::from(limit);
    let mut count = vec![0u32; limit + 1];
    for &d in &depth[..leaves] {
        count[(d as usize).min(limit)] += 1;
    }
    let mut kraft: u64 = (1..=limit).map(|l| u64::from(count[l]) << (limit - l)).sum();
    while kraft > 1 << limit {
        count[limit] -= 1;
        if let Some(l) = (1..limit).rev().find(|&l| count[l] > 0) {
            count[l] -= 1;
            count[l + 1] += 2;
        }
        kraft -= 1;
    }
    // The longest codes go to the rarest symbols.
    let mut at = 0;
    for l in (1..=limit).rev() {
        for _ in 0..count[l] {
            lens[used[at]] = l as u8;
            at += 1;
        }
    }
    lens
}

/// The canonical codes of RFC 1951 3.2.2 for `lens`.
fn canonical(lens: &[u8]) -> Vec<u16> {
    let mut count = [0u16; 16];
    for &l in lens {
        count[usize::from(l)] += 1;
    }
    count[0] = 0;
    let mut next = [0u16; 16];
    let mut code = 0u16;
    for l in 1..16 {
        code = (code + count[l - 1]) << 1;
        next[l] = code;
    }
    lens.iter()
        .map(|&l| {
            if l == 0 {
                return 0;
            }
            let c = next[usize::from(l)];
            next[usize::from(l)] += 1;
            c
        })
        .collect()
}

fn fixed_lengths() -> (Vec<u8>, Vec<u8>) {
    let lit = (0..288)
        .map(|s| match s {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        })
        .collect();
    (lit, vec![5; 30])
}

/// The code-length symbols for the run of lengths `lens`: (symbol, extra bits, their count).
fn length_runs(lens: &[u8]) -> Vec<(u8, u8, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lens.len() {
        let l = lens[i];
        let run = lens[i..].iter().take_while(|&&x| x == l).count();
        let mut left = run;
        if l == 0 {
            while left >= 11 {
                let take = left.min(138);
                out.push((18, (take - 11) as u8, 7));
                left -= take;
            }
            if left >= 3 {
                out.push((17, (left - 3) as u8, 3));
                left = 0;
            }
        } else {
            out.push((l, 0, 0));
            left -= 1;
            while left >= 3 {
                let take = left.min(6);
                out.push((16, (take - 3) as u8, 2));
                left -= take;
            }
        }
        out.extend(std::iter::repeat_n((l, 0, 0), left));
        i += run;
    }
    out
}

/// Bits the tokens take with the given lengths (the end-of-block code included).
fn data_cost(tokens: &[Token], lit: &[u8], dist: &[u8]) -> u64 {
    let mut cost = u64::from(lit[END_OF_BLOCK]);
    for t in tokens {
        cost += match *t {
            Token::Literal(b) => u64::from(lit[usize::from(b)]),
            Token::Match { len, dist: d } => {
                let (li, di) = (len_index(usize::from(len)), dist_index(usize::from(d)));
                u64::from(lit[257 + li]) + u64::from(LEN_EXTRA[li]) + u64::from(dist[di]) + u64::from(DIST_EXTRA[di])
            }
        };
    }
    cost
}

fn write_tokens(bits: &mut Bits, tokens: &[Token], lit: &[u8], dist: &[u8]) {
    let (lit_codes, dist_codes) = (canonical(lit), canonical(dist));
    for t in tokens {
        match *t {
            Token::Literal(b) => bits.code(lit_codes[usize::from(b)], lit[usize::from(b)]),
            Token::Match { len, dist: d } => {
                let (len, d) = (usize::from(len), usize::from(d));
                let (li, di) = (len_index(len), dist_index(d));
                bits.code(lit_codes[257 + li], lit[257 + li]);
                bits.put((len - usize::from(LEN_BASE[li])) as u32, u32::from(LEN_EXTRA[li]));
                bits.code(dist_codes[di], dist[di]);
                bits.put((d - usize::from(DIST_BASE[di])) as u32, u32::from(DIST_EXTRA[di]));
            }
        }
    }
    bits.code(lit_codes[END_OF_BLOCK], lit[END_OF_BLOCK]);
}

/// Writes one block of `tokens`, which cover `raw`, in the shortest of the three forms.
fn block(bits: &mut Bits, tokens: &[Token], raw: &[u8], last: bool) {
    let mut lit_freq = vec![0u32; 286];
    let mut dist_freq = vec![0u32; 30];
    lit_freq[END_OF_BLOCK] = 1;
    for t in tokens {
        match *t {
            Token::Literal(b) => lit_freq[usize::from(b)] += 1,
            Token::Match { len, dist } => {
                lit_freq[257 + len_index(usize::from(len))] += 1;
                dist_freq[dist_index(usize::from(dist))] += 1;
            }
        }
    }
    // Two distance codes at least, so even a block of literals has a complete distance code every decoder takes.
    for d in dist_freq.iter_mut().take(2) {
        *d = (*d).max(1);
    }
    let (dyn_lit, dyn_dist) = (code_lengths(&lit_freq, 15), code_lengths(&dist_freq, 15));
    let hlit = 257.max(dyn_lit.iter().rposition(|&l| l > 0).map_or(0, |p| p + 1));
    let hdist = 1.max(dyn_dist.iter().rposition(|&l| l > 0).map_or(0, |p| p + 1));
    let all: Vec<u8> = dyn_lit[..hlit].iter().chain(&dyn_dist[..hdist]).copied().collect();
    let runs = length_runs(&all);
    let mut cl_freq = [0u32; 19];
    for &(sym, _, _) in &runs {
        cl_freq[usize::from(sym)] += 1;
    }
    let cl = code_lengths(&cl_freq, 7);
    let hclen = 4.max(CL_ORDER.iter().rposition(|&s| cl[s] > 0).map_or(0, |p| p + 1));
    let dynamic_cost = 3
        + 14
        + 3 * hclen as u64
        + runs.iter().map(|&(s, _, n)| u64::from(cl[usize::from(s)]) + u64::from(n)).sum::<u64>()
        + data_cost(tokens, &dyn_lit, &dyn_dist);
    let (fix_lit, fix_dist) = fixed_lengths();
    let fixed_cost = 3 + data_cost(tokens, &fix_lit, &fix_dist);
    // Stored: each piece of at most 65535 bytes costs its header, the padding to a byte (at most 7) and 4 length bytes.
    let pieces = raw.len().div_ceil(65535).max(1) as u64;
    let stored_cost = pieces * (3 + 7 + 32) + 8 * raw.len() as u64;
    if stored_cost < fixed_cost.min(dynamic_cost) {
        let mut chunks = raw.chunks(65535).peekable();
        if chunks.peek().is_none() {
            stored_piece(bits, &[], last);
        }
        while let Some(piece) = chunks.next() {
            stored_piece(bits, piece, last && chunks.peek().is_none());
        }
    } else if fixed_cost <= dynamic_cost {
        bits.put(u32::from(last), 1);
        bits.put(1, 2);
        write_tokens(bits, tokens, &fix_lit, &fix_dist);
    } else {
        bits.put(u32::from(last), 1);
        bits.put(2, 2);
        bits.put((hlit - 257) as u32, 5);
        bits.put((hdist - 1) as u32, 5);
        bits.put((hclen - 4) as u32, 4);
        for &s in &CL_ORDER[..hclen] {
            bits.put(u32::from(cl[s]), 3);
        }
        let cl_codes = canonical(&cl);
        for &(sym, extra, n) in &runs {
            bits.code(cl_codes[usize::from(sym)], cl[usize::from(sym)]);
            bits.put(u32::from(extra), u32::from(n));
        }
        write_tokens(bits, tokens, &dyn_lit, &dyn_dist);
    }
}

fn stored_piece(bits: &mut Bits, piece: &[u8], last: bool) {
    bits.put(u32::from(last), 1);
    bits.put(0, 2);
    bits.align();
    let len = piece.len() as u16;
    bits.out.extend_from_slice(&len.to_le_bytes());
    bits.out.extend_from_slice(&(!len).to_le_bytes());
    bits.out.extend_from_slice(piece);
}

/// An inflater for the tests: RFC 1951 as puff.c reads it (stored, fixed and dynamic blocks), checked against
/// streams Python's zlib wrote, so the compressor's tests do not rest on the compressor's own reading of the format.
#[cfg(test)]
pub(crate) mod inflate {
    use super::{DIST_BASE, DIST_EXTRA, LEN_BASE, LEN_EXTRA, fixed_lengths};

    struct Reader<'a> {
        data: &'a [u8],
        at: usize,
        bit: u32,
    }

    impl Reader<'_> {
        fn bits(&mut self, n: u32) -> Result<u32, String> {
            let mut v = 0;
            for i in 0..n {
                let byte = *self.data.get(self.at).ok_or("out of input")?;
                v |= u32::from((byte >> self.bit) & 1) << i;
                self.bit += 1;
                if self.bit == 8 {
                    self.bit = 0;
                    self.at += 1;
                }
            }
            Ok(v)
        }
    }

    /// Canonical decoding tables: codes per length and the symbols in code order.
    struct Huffman {
        count: [u16; 16],
        symbols: Vec<u16>,
    }

    impl Huffman {
        fn new(lens: &[u8]) -> Result<Self, String> {
            let mut count = [0u16; 16];
            for &l in lens {
                count[usize::from(l)] += 1;
            }
            let mut left = 1i32;
            for &c in &count[1..] {
                left = left * 2 - i32::from(c);
                if left < 0 {
                    return Err("over-subscribed code".into());
                }
            }
            let mut offs = [0u16; 16];
            for l in 1..15 {
                offs[l + 1] = offs[l] + count[l];
            }
            let mut symbols = vec![0u16; lens.len()];
            for (s, &l) in lens.iter().enumerate() {
                if l != 0 {
                    symbols[usize::from(offs[usize::from(l)])] = s as u16;
                    offs[usize::from(l)] += 1;
                }
            }
            count[0] = 0;
            Ok(Self { count, symbols })
        }

        fn decode(&self, r: &mut Reader<'_>) -> Result<usize, String> {
            let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
            for len in 1..16 {
                code |= r.bits(1)? as i32;
                let count = i32::from(self.count[len]);
                if code - count < first {
                    return Ok(usize::from(self.symbols[(index + code - first) as usize]));
                }
                index += count;
                first = (first + count) << 1;
                code <<= 1;
            }
            Err("bad code".into())
        }
    }

    /// The bytes of a zlib stream; the Adler-32 is checked.
    pub(crate) fn zlib(z: &[u8]) -> Result<Vec<u8>, String> {
        if z.len() < 6 || z[0] & 0x0F != 8 || (u16::from(z[0]) << 8 | u16::from(z[1])) % 31 != 0 {
            return Err("not a zlib header".into());
        }
        let mut r = Reader { data: &z[2..], at: 0, bit: 0 };
        let mut out = Vec::new();
        loop {
            let last = r.bits(1)? == 1;
            match r.bits(2)? {
                0 => {
                    if r.bit != 0 {
                        r.bit = 0;
                        r.at += 1;
                    }
                    let head = r.data.get(r.at..r.at + 4).ok_or("short stored header")?;
                    let (len, nlen) = (u16::from_le_bytes([head[0], head[1]]), u16::from_le_bytes([head[2], head[3]]));
                    if len != !nlen {
                        return Err("stored length check".into());
                    }
                    r.at += 4;
                    out.extend_from_slice(r.data.get(r.at..r.at + usize::from(len)).ok_or("short stored block")?);
                    r.at += usize::from(len);
                }
                1 => {
                    let (lit, dist) = fixed_lengths();
                    codes(&mut r, &mut out, &Huffman::new(&lit)?, &Huffman::new(&dist)?)?;
                }
                2 => {
                    let hlit = r.bits(5)? as usize + 257;
                    let hdist = r.bits(5)? as usize + 1;
                    let hclen = r.bits(4)? as usize + 4;
                    let mut cl = [0u8; 19];
                    for &s in &super::CL_ORDER[..hclen] {
                        cl[s] = r.bits(3)? as u8;
                    }
                    let cl = Huffman::new(&cl)?;
                    let mut lens = Vec::with_capacity(hlit + hdist);
                    while lens.len() < hlit + hdist {
                        match cl.decode(&mut r)? {
                            s @ 0..=15 => lens.push(s as u8),
                            16 => {
                                let prev = *lens.last().ok_or("repeat with no length")?;
                                let n = 3 + r.bits(2)?;
                                lens.extend(std::iter::repeat_n(prev, n as usize));
                            }
                            17 => lens.extend(std::iter::repeat_n(0, 3 + r.bits(3)? as usize)),
                            _ => lens.extend(std::iter::repeat_n(0, 11 + r.bits(7)? as usize)),
                        }
                    }
                    if lens.len() != hlit + hdist {
                        return Err("lengths run past the end".into());
                    }
                    codes(&mut r, &mut out, &Huffman::new(&lens[..hlit])?, &Huffman::new(&lens[hlit..])?)?;
                }
                _ => return Err("reserved block type".into()),
            }
            if last {
                break;
            }
        }
        if r.bit != 0 {
            r.at += 1;
        }
        let tail = r.data.get(r.at..r.at + 4).ok_or("no Adler-32")?;
        if u32::from_be_bytes(tail.try_into().unwrap()) != super::adler32(&out) {
            return Err("Adler-32 mismatch".into());
        }
        Ok(out)
    }

    fn codes(r: &mut Reader<'_>, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman) -> Result<(), String> {
        loop {
            let s = lit.decode(r)?;
            if s < 256 {
                out.push(s as u8);
            } else if s == 256 {
                return Ok(());
            } else {
                let i = s - 257;
                if i >= 29 {
                    return Err("bad length symbol".into());
                }
                let len = usize::from(LEN_BASE[i]) + r.bits(u32::from(LEN_EXTRA[i]))? as usize;
                let d = dist.decode(r)?;
                if d >= 30 {
                    return Err("bad distance symbol".into());
                }
                let dist = usize::from(DIST_BASE[d]) + r.bits(u32::from(DIST_EXTRA[d]))? as usize;
                if dist > out.len() {
                    return Err("distance before the start".into());
                }
                for _ in 0..len {
                    out.push(out[out.len() - dist]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample `deflate_fixtures/*.zz` hold, as the script that made them builds it: Python's `zlib.compressobj`
    /// at level 9 with `Z_FIXED` (`fixed.zz`) and the default strategy, which chose a dynamic block (`dynamic.zz`).
    fn sample() -> Vec<u8> {
        let alphabet = b"eeeeeeetttaaooinshrd  \n";
        let mut x: u64 = 1;
        let mut out = Vec::new();
        for _ in 0..3000 {
            x = (x * 1_103_515_245 + 12345) % (1 << 31);
            out.push(alphabet[((x >> 16) % alphabet.len() as u64) as usize]);
        }
        out.extend(std::iter::repeat_n(0u8, 400));
        let again = out[100..900].to_vec();
        out.extend(again);
        out
    }

    #[test]
    fn the_test_inflater_reads_the_fixed_and_dynamic_streams_zlib_wrote() {
        assert_eq!(adler32(&sample()), 0x4c75_8e33, "the sample Python compressed");
        for z in [&include_bytes!("deflate_fixtures/fixed.zz")[..], &include_bytes!("deflate_fixtures/dynamic.zz")[..]]
        {
            assert_eq!(inflate::zlib(z).as_deref(), Ok(&sample()[..]));
        }
        let mut bad = include_bytes!("deflate_fixtures/dynamic.zz").to_vec();
        let at = bad.len() - 1;
        bad[at] ^= 1;
        assert!(inflate::zlib(&bad).is_err(), "a wrong Adler-32 is caught");
    }

    #[test]
    fn adler32_known_values() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(&[]), 1);
        // Past one run of 5552 bytes of 0xFF, where a sum without the modulo would overflow.
        let ones = vec![0xFFu8; 20_000];
        let (mut a, mut b) = (1u64, 0u64);
        for &x in &ones {
            a = (a + u64::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!(adler32(&ones), ((b << 16) | a) as u32);
    }

    /// Review of 22/11/2026, D8: what the compressor writes reads back to the same bytes for content of every
    /// kind (empty, one byte, runs, text, noise that only stored blocks keep small, more than one block).
    #[test]
    fn what_is_compressed_inflates_to_the_same_bytes() {
        let mut noise = Vec::new();
        let mut x: u32 = 0x1234_5678;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            noise.push(x as u8);
        }
        let mut many_blocks = Vec::new();
        for i in 0..400_000u32 {
            many_blocks.push(((i / 7) % 13 + (i % 251) / 50) as u8);
        }
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![42],
            vec![0; 100_000],
            sample(),
            noise.clone(),
            many_blocks,
            b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc".repeat(3000),
        ];
        for data in &cases {
            let z = zlib(data);
            assert_eq!(inflate::zlib(&z).as_deref(), Ok(&data[..]), "{} bytes", data.len());
        }
        assert!(zlib(&noise).len() <= noise.len() + noise.len() / 1000 + 16, "noise grows by the stored headers only");
        assert!(zlib(&[0; 100_000]).len() < 400, "a run is a few bytes");
    }

    /// Review of 22/11/2026, D8: a block whose own codes had to be cut to 15 bits, with literals only (so its distance
    /// code is the forced pair no token uses), still reads back. 18 byte values, value `k` 2^(k+1) times: each outweighs
    /// all the rarer ones together, so the tree is a chain and the rarest would need 18 bits unlimited.
    #[test]
    fn a_dynamic_block_with_clamped_codes_and_no_matches_reads_back() {
        // The values interleaved, though only literals are given to the block anyway.
        let mut left: Vec<usize> = (0..18).map(|k| 2usize << k).collect();
        let mut data = Vec::new();
        while left.iter().any(|&n| n > 0) {
            for (value, n) in left.iter_mut().enumerate() {
                if *n > 0 {
                    data.push(value as u8);
                    *n -= 1;
                }
            }
        }
        assert_eq!(data.len(), (1 << 19) - 2);
        let tokens: Vec<Token> = data.iter().map(|&b| Token::Literal(b)).collect();
        let mut bits = Bits { out: vec![0x78, 0x01], acc: 0, n: 0 };
        block(&mut bits, &tokens, &data, true);
        let mut z = bits.finish();
        z.extend_from_slice(&adler32(&data).to_be_bytes());
        assert_eq!((z[2] >> 1) & 3, 2, "a dynamic block was chosen");
        let mut freq = vec![0u32; 286];
        for &b in &data {
            freq[usize::from(b)] += 1;
        }
        freq[END_OF_BLOCK] = 1;
        assert_eq!(code_lengths(&freq, 30).iter().max(), Some(&18), "unlimited, the chain is 18 deep");
        assert_eq!(code_lengths(&freq, 15).iter().max(), Some(&15), "cut to the limit");
        assert_eq!(inflate::zlib(&z).as_deref(), Ok(&data[..]));
    }

    #[test]
    fn code_lengths_respect_the_limit_and_form_a_complete_code() {
        // Powers of two: each symbol outweighs all the rarer ones together, so the tree is a chain and 30 symbols need
        // 29 bits unlimited. (Fibonacci counts tie at every merge and give a shallow tree.)
        let chain: Vec<u32> = (0..30).map(|k| 1u32 << k).collect();
        assert_eq!(code_lengths(&chain, 30).iter().max(), Some(&29));
        for limit in [7u8, 15] {
            let lens = code_lengths(&chain, limit);
            assert!(lens.iter().all(|&l| (1..=limit).contains(&l)), "{lens:?}");
            let kraft: u64 = lens.iter().map(|&l| 1u64 << (limit - l)).sum();
            assert_eq!(kraft, 1 << limit, "complete for limit {limit}");
        }
        assert_eq!(code_lengths(&[0, 5, 0], 15), vec![1, 1, 0]);
        assert_eq!(code_lengths(&[5, 0, 0], 15), vec![1, 1, 0]);
        assert_eq!(code_lengths(&[0, 0], 15), vec![0, 0]);
    }
}
