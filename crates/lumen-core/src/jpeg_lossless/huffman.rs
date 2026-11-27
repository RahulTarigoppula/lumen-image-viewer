//! Huffman tables: decoding through a lookup table, and optimal tables for
//! writing.

use super::bits::{BitReader, BitWriter};

/// Codes up to this long decode with one table lookup.
const LUT_BITS: u32 = 9;

/// Entropy-coded data that does not decode (an invalid code or value).
#[derive(Clone, Copy, Debug)]
pub(super) struct Bad;

/// A table for reading.
pub(super) struct Decoder {
    /// For every `LUT_BITS`-bit prefix that starts with a code of up to
    /// `LUT_BITS` bits: (code length << 8) | symbol. 0 for longer codes.
    lut: [u16; 1 << LUT_BITS],
    /// Largest code of each length (below the smallest when there is none).
    maxcode: [i32; 17],
    /// Index into `vals` of a code of each length: code + `valoff[length]`.
    valoff: [i32; 17],
    vals: [u8; 256],
    nvals: usize,
}

impl Decoder {
    /// A table from a DHT segment: `counts[i]` codes of length i + 1 for
    /// `symbols` in order. `None` when the counts do not make a prefix code.
    pub(super) fn new(counts: &[u8; 16], symbols: &[u8]) -> Option<Box<Decoder>> {
        let total: usize = counts.iter().map(|&c| c as usize).sum();
        if total > 256 || total != symbols.len() {
            return None;
        }
        let mut d = Box::new(Decoder { lut: [0; 1 << LUT_BITS], maxcode: [-1; 17], valoff: [0; 17], vals: [0; 256], nvals: total });
        d.vals[..total].copy_from_slice(symbols);
        let mut code = 0u32;
        let mut k = 0usize;
        for l in 1..=16u32 {
            let n = counts[l as usize - 1] as u32;
            if code + n > 1 << l {
                return None;
            }
            d.valoff[l as usize] = k as i32 - code as i32;
            for _ in 0..n {
                if l <= LUT_BITS {
                    let shift = LUT_BITS - l;
                    let first = (code << shift) as usize;
                    let entry = ((l as u16) << 8) | symbols[k] as u16;
                    d.lut[first..first + (1usize << shift)].fill(entry);
                }
                code += 1;
                k += 1;
            }
            d.maxcode[l as usize] = code as i32 - 1;
            code <<= 1;
        }
        Some(d)
    }

    /// The next symbol. The reader must hold at least 16 bits (`ensure`).
    #[inline(always)]
    pub(super) fn decode(&self, r: &mut BitReader) -> Result<u8, Bad> {
        let e = self.lut[r.peek(LUT_BITS) as usize];
        if e != 0 {
            r.skip((e >> 8) as u32);
            return Ok(e as u8);
        }
        self.decode_long(r)
    }

    /// Codes longer than the lookup table (T.81 F.2.2.3, DECODE).
    #[cold]
    fn decode_long(&self, r: &mut BitReader) -> Result<u8, Bad> {
        for l in LUT_BITS + 1..=16 {
            let code = r.peek(l) as i32;
            if code <= self.maxcode[l as usize] {
                r.skip(l);
                let i = code + self.valoff[l as usize];
                return if i >= 0 && (i as usize) < self.nvals { Ok(self.vals[i as usize]) } else { Err(Bad) };
            }
        }
        Err(Bad)
    }
}

/// A table to write: `counts[i]` codes of length i + 1 for `symbols` in order
/// (the layout of a DHT segment).
pub(super) struct Spec {
    pub(super) counts: [u8; 16],
    pub(super) symbols: Vec<u8>,
}

/// Codes and lengths of a [`Spec`], by symbol.
pub(super) struct Encoder {
    code: [u16; 256],
    len: [u8; 256],
}

impl Encoder {
    pub(super) fn new(spec: &Spec) -> Encoder {
        let mut e = Encoder { code: [0; 256], len: [0; 256] };
        let mut code = 0u32;
        let mut k = 0usize;
        for l in 1..=16u32 {
            for _ in 0..spec.counts[l as usize - 1] {
                let Some(&s) = spec.symbols.get(k) else { return e };
                e.code[s as usize] = code as u16;
                e.len[s as usize] = l as u8;
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        e
    }

    /// Write `sym` followed by `extra_len` (0..=16) value bits `extra`.
    #[inline(always)]
    pub(super) fn put(&self, w: &mut BitWriter, sym: u8, extra: u32, extra_len: u32) {
        let s = sym as usize;
        w.put(((self.code[s] as u32) << extra_len) | extra, self.len[s] as u32 + extra_len);
    }
}

/// The optimal table for symbol counts `freq`: no code longer than 16 bits
/// and none all ones (T.81 K.2 and K.3, the procedure libjpeg uses).
pub(super) fn optimal(freq: &[u64; 256]) -> Spec {
    let mut f = [0u64; 257];
    f[..256].copy_from_slice(freq);
    if f.iter().all(|&v| v == 0) {
        f[0] = 1;
    }
    // A reserved symbol: it takes the all-ones code, which is then dropped.
    f[256] = 1;
    let mut size = [0usize; 257];
    let mut next = [usize::MAX; 257];
    loop {
        // The two smallest counts (the higher symbol on ties).
        let mut c1 = usize::MAX;
        let mut v = u64::MAX;
        for (i, &x) in f.iter().enumerate() {
            if x != 0 && x <= v {
                v = x;
                c1 = i;
            }
        }
        let mut c2 = usize::MAX;
        let mut v = u64::MAX;
        for (i, &x) in f.iter().enumerate() {
            if x != 0 && x <= v && i != c1 {
                v = x;
                c2 = i;
            }
        }
        if c2 == usize::MAX {
            break;
        }
        f[c1] += f[c2];
        f[c2] = 0;
        let mut a = c1;
        size[a] += 1;
        while next[a] != usize::MAX {
            a = next[a];
            size[a] += 1;
        }
        next[a] = c2;
        let mut b = c2;
        size[b] += 1;
        while next[b] != usize::MAX {
            b = next[b];
            size[b] += 1;
        }
    }
    let mut bits = [0u32; 258];
    for &s in &size {
        if s > 0 {
            bits[s.min(257)] += 1;
        }
    }
    // Shorten codes over 16 bits (K.3, Adjust_BITS).
    for i in (17..258).rev() {
        while bits[i] > 0 {
            let mut j = i - 2;
            while j > 0 && bits[j] == 0 {
                j -= 1;
            }
            if j == 0 || bits[i] < 2 {
                // Cannot happen for a real code tree; stop rather than loop.
                bits[16] += bits[i];
                bits[i] = 0;
                break;
            }
            bits[i] -= 2;
            bits[i - 1] += 1;
            bits[j + 1] += 2;
            bits[j] -= 1;
        }
    }
    // Drop the reserved code (one of the longest).
    let mut i = 16;
    while i > 0 && bits[i] == 0 {
        i -= 1;
    }
    if i > 0 {
        bits[i] -= 1;
    }
    let mut counts = [0u8; 16];
    for l in 1..=16 {
        counts[l - 1] = bits[l].min(255) as u8;
    }
    // Symbols by their original code length, then by value.
    let mut symbols = Vec::new();
    for l in 1..=256 {
        for (s, &sz) in size.iter().enumerate().take(256) {
            if sz == l {
                symbols.push(s as u8);
            }
        }
    }
    Spec { counts, symbols }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kraft sum of a spec: exactly a full code minus the dropped all-ones code.
    fn check(spec: &Spec, used: usize) {
        let n: usize = spec.counts.iter().map(|&c| c as usize).sum();
        assert_eq!(n, spec.symbols.len());
        assert_eq!(n, used);
        let kraft: u64 = spec.counts.iter().enumerate().map(|(i, &c)| (c as u64) << (16 - (i + 1))).sum();
        assert!(kraft < 1 << 16, "{kraft}");
        // Decodable: build the reader table.
        assert!(Decoder::new(&spec.counts, &spec.symbols).is_some());
    }

    #[test]
    fn optimal_tables() {
        let mut f = [0u64; 256];
        f[0] = 1000;
        f[1] = 10;
        f[0x11] = 3;
        check(&optimal(&f), 3);
        // One symbol still gets a 1-bit code.
        let mut one = [0u64; 256];
        one[5] = 42;
        let s = optimal(&one);
        check(&s, 1);
        assert_eq!(s.counts[0], 1);
        // Nothing at all: a dummy symbol.
        check(&optimal(&[0; 256]), 1);
        // Fibonacci counts make a very deep tree that must be cut to 16 bits.
        let mut fib = [0u64; 256];
        let (mut a, mut b) = (1u64, 1u64);
        for v in fib.iter_mut().take(40) {
            *v = a;
            (a, b) = (b, a + b);
        }
        let s = optimal(&fib);
        check(&s, 40);
        // All 256 symbols.
        let all: [u64; 256] = std::array::from_fn(|i| 1 + i as u64 * 7);
        check(&optimal(&all), 256);
    }

    #[test]
    fn bad_tables_are_refused() {
        // Three 1-bit codes cannot exist.
        let mut counts = [0u8; 16];
        counts[0] = 3;
        assert!(Decoder::new(&counts, &[1, 2, 3]).is_none());
        // Symbol count mismatch.
        counts[0] = 1;
        assert!(Decoder::new(&counts, &[1, 2]).is_none());
    }

    #[test]
    fn encode_decode_codes() {
        let mut f = [0u64; 256];
        for (i, v) in f.iter_mut().enumerate().take(162) {
            *v = (i as u64 * 37 % 101) + 1;
        }
        let spec = optimal(&f);
        let enc = Encoder::new(&spec);
        let dec = Decoder::new(&spec.counts, &spec.symbols).unwrap();
        let mut w = BitWriter::new(Vec::new());
        let syms: Vec<u8> = (0..162u32).chain((0..162).rev()).map(|s| s as u8).collect();
        for &s in &syms {
            enc.put(&mut w, s, 5, 3);
        }
        w.align();
        // Unstuff (the writer stuffs a zero after 0xFF).
        let mut data = Vec::new();
        let mut i = 0;
        while i < w.out.len() {
            data.push(w.out[i]);
            if w.out[i] == 0xFF {
                i += 1;
            }
            i += 1;
        }
        let mut r = BitReader::new(&data);
        for &s in &syms {
            r.ensure();
            assert_eq!(dec.decode(&mut r).unwrap(), s);
            assert_eq!(r.bits(3), 5);
        }
        assert!(!r.overrun());
    }
}
