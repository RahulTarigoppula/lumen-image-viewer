//! Writing coefficients out as a baseline JPEG with optimal Huffman tables.

use super::bits::BitWriter;
use super::huffman::{optimal, Encoder, Spec};
use super::Jpeg;
use anyhow::{anyhow, bail, Result};

const OUT_OF_RANGE: &str = "This JPEG has coefficient values out of range";

/// Where the Huffman coding of the blocks goes: counted first, then written.
trait Sink {
    /// One Huffman symbol of table `t` (0 for the first component, 1 for the
    /// others) followed by `extra_len` value bits.
    fn symbol(&mut self, dc: bool, t: usize, sym: u8, extra: u32, extra_len: u32);
    fn restart(&mut self, n: u8);
}

struct Counter {
    dc: [[u64; 256]; 2],
    ac: [[u64; 256]; 2],
}

impl Sink for Counter {
    #[inline(always)]
    fn symbol(&mut self, dc: bool, t: usize, sym: u8, _: u32, _: u32) {
        if dc {
            self.dc[t][sym as usize] += 1;
        } else {
            self.ac[t][sym as usize] += 1;
        }
    }
    fn restart(&mut self, _: u8) {}
}

struct Emitter<'a> {
    w: BitWriter,
    dc: &'a [Encoder],
    ac: &'a [Encoder],
}

impl Sink for Emitter<'_> {
    #[inline(always)]
    fn symbol(&mut self, dc: bool, t: usize, sym: u8, extra: u32, extra_len: u32) {
        let e = if dc { &self.dc[t] } else { &self.ac[t] };
        e.put(&mut self.w, sym, extra, extra_len);
    }
    fn restart(&mut self, n: u8) {
        self.w.restart(n);
    }
}

/// Size category and value bits of a coefficient (T.81 F.1.2.1).
#[inline(always)]
fn magnitude(v: i32) -> (u32, u32) {
    let size = 32 - v.unsigned_abs().leading_zeros();
    let bits = (if v < 0 { v - 1 } else { v }) as u32 & ((1u32 << size) - 1);
    (size, bits)
}

/// Huffman-code one block (sequential mode, T.81 F.1.2).
#[inline(always)]
fn encode_block<S: Sink>(s: &mut S, t: usize, blk: &[i16; 64], pred: &mut i32) -> Result<()> {
    let dc = blk[0] as i32;
    let (size, bits) = magnitude(dc - *pred);
    *pred = dc;
    if size > 11 {
        bail!(OUT_OF_RANGE);
    }
    s.symbol(true, t, size as u8, bits, size);
    let mut run = 0u32;
    for &c in &blk[1..] {
        if c == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            s.symbol(false, t, 0xF0, 0, 0);
            run -= 16;
        }
        let (size, bits) = magnitude(c as i32);
        if size > 10 {
            bail!(OUT_OF_RANGE);
        }
        s.symbol(false, t, ((run << 4) | size) as u8, bits, size);
        run = 0;
    }
    if run > 0 {
        s.symbol(false, t, 0x00, 0, 0);
    }
    Ok(())
}

/// The scans to write: one interleaved scan when the components fit in one
/// (at most 10 blocks per MCU), otherwise one scan per component.
fn scan_plan(j: &Jpeg) -> Vec<Vec<usize>> {
    let n = j.comps.len();
    if n > 1 && j.comps.iter().map(|c| c.h * c.v).sum::<usize>() <= 10 {
        vec![(0..n).collect()]
    } else {
        (0..n).map(|i| vec![i]).collect()
    }
}

/// Code the blocks of one scan (components `comps`) into `s`.
fn run_scan<S: Sink>(j: &Jpeg, comps: &[usize], s: &mut S) -> Result<()> {
    let single = comps.len() == 1;
    let (per_row, rows) = if single { j.real_blocks(&j.comps[comps[0]]) } else { j.mcus() };
    let interval = j.restart as usize;
    let mut pred = [0i32; 4];
    let mut rst = 0u8;
    for mcu in 0..per_row * rows {
        if interval > 0 && mcu > 0 && mcu % interval == 0 {
            s.restart(rst);
            rst = (rst + 1) & 7;
            pred = [0; 4];
        }
        let (mx, my) = (mcu % per_row, mcu / per_row);
        for (i, &ci) in comps.iter().enumerate() {
            let c = &j.comps[ci];
            let t = usize::from(ci > 0);
            let (nh, nv) = if single { (1, 1) } else { (c.h, c.v) };
            for v in 0..nv {
                for h in 0..nh {
                    let blk = c.blocks.get((my * nv + v) * c.bw + mx * nh + h).ok_or_else(|| anyhow!(super::decode::DAMAGED))?;
                    encode_block(s, t, blk, &mut pred[i])?;
                }
            }
        }
    }
    Ok(())
}

fn put_segment(out: &mut Vec<u8>, marker: u8, body: &[u8]) -> Result<()> {
    if body.len() > 65533 {
        bail!("A metadata block is too large for a JPEG file");
    }
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
    Ok(())
}

fn put_table(out: &mut Vec<u8>, class: u8, id: usize, spec: &Spec) {
    out.push((class << 4) | id as u8);
    out.extend_from_slice(&spec.counts);
    out.extend_from_slice(&spec.symbols);
}

/// The JPEG file for `j`: its APPn / COM segments in order, quantization
/// tables, a baseline frame (extended sequential when a quantization table
/// needs 16-bit entries), optimal Huffman tables, the scans, then the trailer.
pub(super) fn write(j: &Jpeg) -> Result<Vec<u8>> {
    let blocks: usize = j.comps.iter().map(|c| c.blocks.len()).sum();
    let mut out = Vec::with_capacity(blocks * 12 + j.trailer.len() + 4096);
    out.extend_from_slice(&[0xFF, 0xD8]);
    for (m, body) in &j.segments {
        put_segment(&mut out, *m, body)?;
    }
    // Each distinct quantization table once.
    let mut tables: Vec<[u16; 64]> = Vec::new();
    let mut tq = Vec::with_capacity(j.comps.len());
    for c in &j.comps {
        let q = c.q.ok_or_else(|| anyhow!(super::decode::DAMAGED))?;
        let i = match tables.iter().position(|t| *t == q) {
            Some(i) => i,
            None => {
                tables.push(q);
                tables.len() - 1
            }
        };
        tq.push(i as u8);
    }
    let wide = tables.iter().any(|t| t.iter().any(|&v| v > 255));
    let mut dqt = Vec::new();
    for (i, t) in tables.iter().enumerate() {
        dqt.push((u8::from(wide) << 4) | i as u8);
        for &v in t {
            if wide {
                dqt.extend_from_slice(&v.to_be_bytes());
            } else {
                dqt.push(v as u8);
            }
        }
    }
    put_segment(&mut out, 0xDB, &dqt)?;
    let mut sof = vec![8];
    sof.extend_from_slice(&(j.height as u16).to_be_bytes());
    sof.extend_from_slice(&(j.width as u16).to_be_bytes());
    sof.push(j.comps.len() as u8);
    for (c, &q) in j.comps.iter().zip(&tq) {
        sof.extend_from_slice(&[c.id, ((c.h as u8) << 4) | c.v as u8, q]);
    }
    put_segment(&mut out, if wide { 0xC1 } else { 0xC0 }, &sof)?;
    // Optimal Huffman tables from the symbols the scans will use.
    let scans = scan_plan(j);
    let mut count = Box::new(Counter { dc: [[0; 256]; 2], ac: [[0; 256]; 2] });
    for sc in &scans {
        run_scan(j, sc, &mut *count)?;
    }
    let groups = if j.comps.len() > 1 { 2 } else { 1 };
    let (mut dc_enc, mut ac_enc, mut dht) = (Vec::new(), Vec::new(), Vec::new());
    for t in 0..groups {
        let dc = optimal(&count.dc[t]);
        let ac = optimal(&count.ac[t]);
        put_table(&mut dht, 0, t, &dc);
        put_table(&mut dht, 1, t, &ac);
        dc_enc.push(Encoder::new(&dc));
        ac_enc.push(Encoder::new(&ac));
    }
    put_segment(&mut out, 0xC4, &dht)?;
    if j.restart > 0 {
        put_segment(&mut out, 0xDD, &j.restart.to_be_bytes())?;
    }
    let mut e = Emitter { w: BitWriter::new(out), dc: &dc_enc, ac: &ac_enc };
    for sc in &scans {
        let mut sos = vec![sc.len() as u8];
        for &ci in sc {
            let t = u8::from(ci > 0);
            sos.extend_from_slice(&[j.comps[ci].id, (t << 4) | t]);
        }
        sos.extend_from_slice(&[0, 63, 0]);
        put_segment(&mut e.w.out, 0xDA, &sos)?;
        run_scan(j, sc, &mut e)?;
        e.w.align();
    }
    let mut out = e.w.out;
    out.extend_from_slice(&[0xFF, 0xD9]);
    out.extend_from_slice(&j.trailer);
    Ok(out)
}
