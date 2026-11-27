//! Bit reading and writing for JPEG entropy-coded data.

/// Reads bits, most significant first, from entropy-coded data whose stuffed
/// zero bytes were already removed. Past the end it reads zero bits;
/// [`BitReader::overrun`] tells when that happened.
pub(super) struct BitReader<'a> {
    data: &'a [u8],
    /// Next byte to load.
    pos: usize,
    /// Buffered bits, left aligned. The bits below the `n` valid ones are
    /// zero or already the stream's next bits (a refill loads a whole word
    /// and counts only the bytes that fit), so OR-ing those bytes in again
    /// later changes nothing.
    acc: u64,
    n: u32,
}

impl<'a> BitReader<'a> {
    pub(super) fn new(data: &'a [u8]) -> BitReader<'a> {
        let mut r = BitReader { data, pos: 0, acc: 0, n: 0 };
        r.refill();
        r
    }

    /// Top the buffer up to 56..=63 bits.
    #[inline(always)]
    fn refill(&mut self) {
        match self.data.get(self.pos..self.pos + 8) {
            Some(w) => {
                let w = u64::from_be_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]);
                self.acc |= w >> self.n;
                let k = (63 - self.n) >> 3;
                self.pos += k as usize;
                self.n += k << 3;
            }
            None => self.refill_slow(),
        }
    }

    /// Near the end: byte by byte, zeros past it.
    #[cold]
    fn refill_slow(&mut self) {
        while self.n < 56 {
            let b = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.acc |= (b as u64) << (56 - self.n);
            self.n += 8;
        }
    }

    /// At least 32 bits buffered: room for one Huffman code (up to 16 bits)
    /// and the value bits that follow it (up to 15).
    #[inline(always)]
    pub(super) fn ensure(&mut self) {
        if self.n < 32 {
            self.refill();
        }
    }

    /// The next `k` (1..=32) bits, not consumed.
    #[inline(always)]
    pub(super) fn peek(&self, k: u32) -> u32 {
        (self.acc >> (64 - k)) as u32
    }

    /// Drop `k` bits (no more than are buffered).
    #[inline(always)]
    pub(super) fn skip(&mut self, k: u32) {
        self.acc <<= k;
        self.n -= k;
    }

    /// The next `k` (0..=16) bits as a number; they must be buffered.
    #[inline(always)]
    pub(super) fn bits(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        let v = self.peek(k);
        self.skip(k);
        v
    }

    /// A `k`-bit value with its sign (T.81 F.2.2.1, EXTEND).
    #[inline(always)]
    pub(super) fn signed(&mut self, k: u32) -> i32 {
        if k == 0 {
            return 0;
        }
        let v = self.bits(k) as i32;
        if v < 1 << (k - 1) {
            v - (1 << k) + 1
        } else {
            v
        }
    }

    /// One bit, refilling when needed (refinement scans read many in a row).
    #[inline(always)]
    pub(super) fn bit(&mut self) -> bool {
        if self.n == 0 {
            self.refill();
        }
        let b = self.acc >> 63 != 0;
        self.skip(1);
        b
    }

    /// More bits were used than the data holds: it ends early.
    pub(super) fn overrun(&self) -> bool {
        self.pos as u64 * 8 > self.data.len() as u64 * 8 + self.n as u64
    }
}

/// Writes bits, most significant first, stuffing a zero byte after each 0xFF.
pub(super) struct BitWriter {
    pub(super) out: Vec<u8>,
    acc: u64,
    /// Bits in `acc` not written yet (below 32 between calls).
    n: u32,
}

impl BitWriter {
    pub(super) fn new(out: Vec<u8>) -> BitWriter {
        BitWriter { out, acc: 0, n: 0 }
    }

    /// Append the low `len` (0..=32) bits of `v`.
    #[inline(always)]
    pub(super) fn put(&mut self, v: u32, len: u32) {
        self.acc = (self.acc << len) | (v as u64 & ((1u64 << len) - 1));
        self.n += len;
        if self.n >= 32 {
            self.flush();
        }
    }

    /// Write out every complete byte.
    #[inline]
    fn flush(&mut self) {
        while self.n >= 8 {
            self.n -= 8;
            let b = (self.acc >> self.n) as u8;
            self.out.push(b);
            if b == 0xFF {
                self.out.push(0);
            }
        }
    }

    /// Pad to a whole byte with 1 bits, as the standard asks, and write it.
    pub(super) fn align(&mut self) {
        let pad = (8 - self.n % 8) % 8;
        self.put((1 << pad) - 1, pad);
        self.flush();
    }

    /// End a restart interval with marker RSTn.
    pub(super) fn restart(&mut self, n: u8) {
        self.align();
        self.out.push(0xFF);
        self.out.push(0xD0 + (n & 7));
    }
}
