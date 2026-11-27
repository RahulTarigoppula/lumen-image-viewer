//! Reading a JPEG into its quantized DCT coefficients (T.81 baseline,
//! extended and progressive Huffman modes, 8-bit).

use super::bits::BitReader;
use super::huffman::{Bad, Decoder};
use super::{Component, CropGrid, Jpeg};
use anyhow::{anyhow, bail, Result};

pub(super) const DAMAGED: &str = "This JPEG is damaged";
pub(super) const INCOMPLETE: &str = "This JPEG is incomplete (the file ends early)";
/// Coefficient blocks held in memory at most (128 bytes each, 1.6 GB).
const MAX_BLOCKS: u64 = 12 << 20;

/// The frame size, sampling and EXIF orientation, read without the picture.
pub(super) struct Header {
    width: u32,
    height: u32,
    hmax: u32,
    vmax: u32,
    orientation: u8,
}

impl Header {
    pub(super) fn grid(&self) -> CropGrid {
        CropGrid { stored: (self.width, self.height), orientation: self.orientation, step: (8 * self.hmax, 8 * self.vmax) }
    }
}

/// Refuse the kinds of JPEG lossless editing cannot handle.
fn check_supported(m: u8) -> Result<()> {
    match m {
        0xC9..=0xCB | 0xCD..=0xCF | 0xCC => bail!("This JPEG uses arithmetic coding, which lossless editing does not support"),
        0xC3 | 0xC5..=0xC7 | 0xDE | 0xDF => bail!("This kind of JPEG (lossless or hierarchical) cannot be edited losslessly"),
        _ => Ok(()),
    }
}

/// Markers that have no length field.
fn standalone(m: u8) -> bool {
    matches!(m, 0x01 | 0xD0..=0xD9)
}

/// The next marker at or after `pos` and the position after it. Stray bytes
/// before it and fill bytes (0xFF) are skipped.
fn next_marker(data: &[u8], mut pos: usize) -> Option<(u8, usize)> {
    loop {
        while pos < data.len() && data[pos] != 0xFF {
            pos += 1;
        }
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        let m = *data.get(pos)?;
        pos += 1;
        // FF 00 outside entropy-coded data is not a marker.
        if m != 0 {
            return Some((m, pos));
        }
    }
}

/// The payload of the segment whose length field is at `pos`, and the
/// position after it.
fn segment(data: &[u8], pos: usize) -> Result<(&[u8], usize)> {
    let len = data.get(pos..pos + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(|| anyhow!(INCOMPLETE))?;
    if len < 2 {
        bail!(DAMAGED);
    }
    let end = pos + len;
    if end > data.len() {
        bail!(INCOMPLETE);
    }
    Ok((&data[pos + 2..end], end))
}

/// A SOF0 / SOF1 / SOF2 frame header: width, height and components.
fn frame(b: &[u8]) -> Result<(u32, u32, Vec<Component>)> {
    if b.len() < 6 {
        bail!(DAMAGED);
    }
    if b[0] != 8 {
        bail!("{}-bit JPEGs cannot be edited losslessly", b[0]);
    }
    let height = u16::from_be_bytes([b[1], b[2]]) as u32;
    let width = u16::from_be_bytes([b[3], b[4]]) as u32;
    let n = b[5] as usize;
    if height == 0 {
        bail!("This JPEG gives its height after the picture, which lossless editing does not support");
    }
    if n > 4 {
        bail!("JPEGs with more than four color channels cannot be edited losslessly");
    }
    if width == 0 || n == 0 || b.len() < 6 + 3 * n {
        bail!(DAMAGED);
    }
    let mut comps: Vec<Component> = Vec::with_capacity(n);
    for i in 0..n {
        let c = &b[6 + 3 * i..9 + 3 * i];
        let (h, v, tq) = ((c[1] >> 4) as usize, (c[1] & 15) as usize, c[2] as usize);
        if !(1..=4).contains(&h) || !(1..=4).contains(&v) || tq > 3 || comps.iter().any(|o| o.id == c[0]) {
            bail!(DAMAGED);
        }
        comps.push(Component { id: c[0], h, v, tq, q: None, bw: 0, bh: 0, blocks: Vec::new() });
    }
    if n == 1 {
        // A single component is always coded block by block, whatever its
        // sampling factors say.
        comps[0].h = 1;
        comps[0].v = 1;
    }
    Ok((width, height, comps))
}

/// The frame header and EXIF orientation of `data`. `None` when the data
/// ends before the frame header (only the start of a file was read).
pub(super) fn headers(data: &[u8]) -> Result<Option<Header>> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        bail!("This is not a JPEG file");
    }
    let mut orientation = 1;
    let mut pos = 2;
    while let Some((m, p)) = next_marker(data, pos) {
        pos = p;
        check_supported(m)?;
        if standalone(m) {
            if m >= 0xD8 {
                bail!(DAMAGED);
            }
            continue;
        }
        let Some(len) = data.get(pos..pos + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize) else { return Ok(None) };
        if len < 2 {
            bail!(DAMAGED);
        }
        if pos + len > data.len() {
            return Ok(None);
        }
        let body = &data[pos + 2..pos + len];
        pos += len;
        match m {
            0xE1 => {
                if let Some(o) = super::meta::exif_tiff(body).and_then(super::meta::orientation).filter(|o| (1..=8).contains(o)) {
                    orientation = o as u8;
                }
            }
            0xC0..=0xC2 => {
                let (width, height, comps) = frame(body)?;
                let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1) as u32;
                let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1) as u32;
                return Ok(Some(Header { width, height, hmax, vmax, orientation }));
            }
            0xDA => bail!(DAMAGED),
            _ => {}
        }
    }
    Ok(None)
}

/// Tables in effect while reading.
struct Tables {
    q: [Option<[u16; 64]>; 4],
    dc: [Option<Box<Decoder>>; 4],
    ac: [Option<Box<Decoder>>; 4],
    restart: u16,
}

impl Tables {
    fn dqt(&mut self, mut b: &[u8]) -> Result<()> {
        while let Some(&pt) = b.first() {
            let (pq, tq) = (pt >> 4, (pt & 15) as usize);
            let n = if pq == 0 { 64 } else { 128 };
            if pq > 1 || tq > 3 || b.len() < 1 + n {
                bail!(DAMAGED);
            }
            let mut t = [0u16; 64];
            for (k, v) in t.iter_mut().enumerate() {
                *v = if pq == 0 { b[1 + k] as u16 } else { u16::from_be_bytes([b[1 + 2 * k], b[2 + 2 * k]]) };
            }
            self.q[tq] = Some(t);
            b = &b[1 + n..];
        }
        Ok(())
    }

    fn dht(&mut self, mut b: &[u8]) -> Result<()> {
        while !b.is_empty() {
            if b.len() < 17 {
                bail!(DAMAGED);
            }
            let (tc, th) = (b[0] >> 4, (b[0] & 15) as usize);
            let mut counts = [0u8; 16];
            counts.copy_from_slice(&b[1..17]);
            let total: usize = counts.iter().map(|&c| c as usize).sum();
            if tc > 1 || th > 3 || b.len() < 17 + total {
                bail!(DAMAGED);
            }
            let table = Decoder::new(&counts, &b[17..17 + total]).ok_or_else(|| anyhow!(DAMAGED))?;
            if tc == 0 {
                self.dc[th] = Some(table);
            } else {
                self.ac[th] = Some(table);
            }
            b = &b[17 + total..];
        }
        Ok(())
    }
}

/// One component of a scan: its frame index and Huffman tables.
struct ScanComp {
    ci: usize,
    dc: usize,
    ac: usize,
}

struct Scan {
    comps: Vec<ScanComp>,
    /// Spectral selection (zigzag indices) and successive approximation.
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sequential,
    DcFirst,
    DcRefine,
    AcFirst,
    AcRefine,
}

impl Scan {
    fn read(b: &[u8], j: &Jpeg) -> Result<Scan> {
        let n = *b.first().ok_or_else(|| anyhow!(DAMAGED))? as usize;
        if n == 0 || n > 4 || b.len() < 4 + 2 * n {
            bail!(DAMAGED);
        }
        let mut comps: Vec<ScanComp> = Vec::with_capacity(n);
        for i in 0..n {
            let (id, t) = (b[1 + 2 * i], b[2 + 2 * i]);
            let ci = j.comps.iter().position(|c| c.id == id).ok_or_else(|| anyhow!(DAMAGED))?;
            let (dc, ac) = ((t >> 4) as usize, (t & 15) as usize);
            if dc > 3 || ac > 3 || comps.iter().any(|s| s.ci == ci) {
                bail!(DAMAGED);
            }
            comps.push(ScanComp { ci, dc, ac });
        }
        let p = 1 + 2 * n;
        let (mut ss, mut se, mut ah, mut al) = (b[p] as usize, b[p + 1] as usize, (b[p + 2] >> 4) as u32, (b[p + 2] & 15) as u32);
        if j.progressive {
            let ok = ss <= se && se <= 63 && (ss > 0 || se == 0) && (ss == 0 || n == 1) && al <= 13 && (ah == 0 || ah == al + 1);
            if !ok {
                bail!(DAMAGED);
            }
        } else {
            // Sequential scans always carry every coefficient (libjpeg
            // ignores odd values here too).
            (ss, se, ah, al) = (0, 63, 0, 0);
        }
        if n > 1 && comps.iter().map(|s| j.comps[s.ci].h * j.comps[s.ci].v).sum::<usize>() > 10 {
            bail!(DAMAGED);
        }
        Ok(Scan { comps, ss, se, ah, al })
    }

    fn kind(&self, progressive: bool) -> Kind {
        match (progressive, self.ss == 0, self.ah == 0) {
            (false, _, _) => Kind::Sequential,
            (true, true, true) => Kind::DcFirst,
            (true, true, false) => Kind::DcRefine,
            (true, false, true) => Kind::AcFirst,
            (true, false, false) => Kind::AcRefine,
        }
    }
}

/// Allocate the coefficient blocks once the first scan starts, after checking
/// the frame size against the memory limit and the amount of data.
fn allocate(j: &mut Jpeg, data_len: usize) -> Result<()> {
    let (mx, my) = j.mcus();
    let (mut real, mut padded) = (0u64, 0u64);
    for c in &j.comps {
        let (rw, rh) = j.real_blocks(c);
        real += (rw * rh) as u64;
        padded += (mx * c.h * my * c.v) as u64;
    }
    if padded > MAX_BLOCKS {
        bail!("This picture is too large to edit losslessly");
    }
    // Every block takes at least one bit of entropy-coded data: a header
    // promising far more blocks than that is damaged (and must not make us
    // allocate gigabytes).
    if real > data_len as u64 * 8 {
        bail!(DAMAGED);
    }
    for c in &mut j.comps {
        c.bw = mx * c.h;
        c.bh = my * c.v;
        c.blocks = vec![[0i16; 64]; c.bw * c.bh];
    }
    Ok(())
}

/// Copy the entropy-coded data at `pos` into `out` without its stuffed zero
/// bytes, noting in `starts` where each restart interval begins. Returns the
/// position of the marker that ends the scan (or the end of the data).
fn unstuff(data: &[u8], mut pos: usize, out: &mut Vec<u8>, starts: &mut Vec<usize>) -> usize {
    out.clear();
    starts.clear();
    starts.push(0);
    loop {
        let rest = &data[pos..];
        let Some(i) = rest.iter().position(|&b| b == 0xFF) else {
            out.extend_from_slice(rest);
            return data.len();
        };
        out.extend_from_slice(&rest[..i]);
        pos += i;
        let mut q = pos + 1;
        while q < data.len() && data[q] == 0xFF {
            q += 1;
        }
        match data.get(q).copied() {
            None => return data.len(),
            Some(0x00) => {
                out.push(0xFF);
                pos = q + 1;
            }
            Some(0xD0..=0xD7) => {
                starts.push(out.len());
                pos = q + 1;
            }
            Some(_) => return q - 1,
        }
    }
}

/// Read the JPEG `data` into coefficients.
pub(super) fn parse(data: &[u8]) -> Result<Jpeg> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        bail!("This is not a JPEG file");
    }
    let mut t = Tables { q: [None; 4], dc: Default::default(), ac: Default::default(), restart: 0 };
    let mut jpeg: Option<Jpeg> = None;
    let mut segments: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut trailer: Option<Vec<u8>> = None;
    let mut scans = 0usize;
    let (mut buf, mut starts) = (Vec::new(), Vec::new());
    let mut pos = 2;
    while let Some((m, p)) = next_marker(data, pos) {
        pos = p;
        check_supported(m)?;
        match m {
            0xD9 => {
                trailer = Some(data[pos..].to_vec());
                break;
            }
            0xD8 => bail!(DAMAGED),
            _ if standalone(m) => continue,
            _ => {}
        }
        let (body, end) = segment(data, pos)?;
        pos = end;
        match m {
            0xE0..=0xEF | 0xFE => segments.push((m, body.to_vec())),
            0xDB => t.dqt(body)?,
            0xC4 => t.dht(body)?,
            0xDD => {
                if body.len() < 2 {
                    bail!(DAMAGED);
                }
                t.restart = u16::from_be_bytes([body[0], body[1]]);
            }
            0xC0..=0xC2 => {
                if jpeg.is_some() {
                    bail!(DAMAGED);
                }
                let (width, height, comps) = frame(body)?;
                jpeg = Some(Jpeg { width, height, comps, restart: 0, progressive: m == 0xC2, segments: Vec::new(), trailer: Vec::new() });
            }
            0xDA => {
                let Some(j) = jpeg.as_mut() else { bail!(DAMAGED) };
                let scan = Scan::read(body, j)?;
                if scans == 0 {
                    allocate(j, data.len())?;
                    j.restart = t.restart;
                }
                for sc in &scan.comps {
                    let c = &mut j.comps[sc.ci];
                    if c.q.is_none() {
                        c.q = Some(t.q[c.tq].ok_or_else(|| anyhow!(DAMAGED))?);
                    }
                }
                pos = unstuff(data, pos, &mut buf, &mut starts);
                t.decode_scan(j, &scan, &buf, &starts, pos >= data.len())?;
                scans += 1;
            }
            _ => {}
        }
    }
    let Some(mut j) = jpeg else { bail!(DAMAGED) };
    let Some(trailer) = trailer else { bail!(INCOMPLETE) };
    if scans == 0 {
        bail!(DAMAGED);
    }
    j.segments = segments;
    j.trailer = trailer;
    Ok(j)
}

impl Tables {
    /// Decode one scan's entropy-coded data `buf` (restart intervals start
    /// at `starts`) into the blocks of `j`. `at_end`: the file ends with it.
    fn decode_scan(&self, j: &mut Jpeg, s: &Scan, buf: &[u8], starts: &[usize], at_end: bool) -> Result<()> {
        let kind = s.kind(j.progressive);
        let single = s.comps.len() == 1;
        let (per_row, rows) = if single { j.real_blocks(&j.comps[s.comps[0].ci]) } else { j.mcus() };
        let total = per_row * rows;
        let mut tables: Vec<(Option<&Decoder>, Option<&Decoder>)> = Vec::with_capacity(s.comps.len());
        for sc in &s.comps {
            let dc = self.dc[sc.dc].as_deref();
            let ac = self.ac[sc.ac].as_deref();
            let need_dc = matches!(kind, Kind::Sequential | Kind::DcFirst);
            let need_ac = matches!(kind, Kind::Sequential | Kind::AcFirst | Kind::AcRefine);
            if (need_dc && dc.is_none()) || (need_ac && ac.is_none()) {
                bail!(DAMAGED);
            }
            tables.push((dc, ac));
        }
        let ended = || anyhow!(if at_end { INCOMPLETE } else { DAMAGED });
        let interval = if self.restart > 0 { self.restart as usize } else { total.max(1) };
        let mut mcu = 0usize;
        let mut seg = 0usize;
        while mcu < total {
            let from = starts.get(seg).copied().unwrap_or(buf.len());
            let to = starts.get(seg + 1).copied().unwrap_or(buf.len()).max(from);
            let mut r = BitReader::new(&buf[from..to]);
            let mut pred = [0i32; 4];
            let mut eobrun = 0u32;
            let end = (mcu + interval).min(total);
            while mcu < end {
                let (mx, my) = (mcu % per_row, mcu / per_row);
                for (i, sc) in s.comps.iter().enumerate() {
                    let c = &mut j.comps[sc.ci];
                    let (nh, nv) = if single { (1, 1) } else { (c.h, c.v) };
                    for v in 0..nv {
                        for h in 0..nh {
                            let blk = &mut c.blocks[(my * nv + v) * c.bw + mx * nh + h];
                            block(kind, &mut r, tables[i], s, &mut pred[i], &mut eobrun, blk).map_err(|_| anyhow!(DAMAGED))?;
                        }
                    }
                }
                mcu += 1;
                if mcu % 1024 == 0 && r.overrun() {
                    return Err(ended());
                }
            }
            if r.overrun() {
                return Err(ended());
            }
            seg += 1;
        }
        Ok(())
    }
}

/// Decode one block of a scan of `kind`.
#[inline(always)]
fn block(kind: Kind, r: &mut BitReader, (dc, ac): (Option<&Decoder>, Option<&Decoder>), s: &Scan, pred: &mut i32, eobrun: &mut u32, blk: &mut [i16; 64]) -> Result<(), Bad> {
    match kind {
        Kind::Sequential => sequential(r, dc.ok_or(Bad)?, ac.ok_or(Bad)?, pred, blk),
        Kind::DcFirst => dc_first(r, dc.ok_or(Bad)?, s.al, pred, blk),
        Kind::DcRefine => {
            if r.bit() {
                blk[0] |= 1 << s.al;
            }
            Ok(())
        }
        Kind::AcFirst => ac_first(r, ac.ok_or(Bad)?, s, eobrun, blk),
        Kind::AcRefine => ac_refine(r, ac.ok_or(Bad)?, s, eobrun, blk),
    }
}

/// A baseline / extended sequential block: the DC difference, then AC
/// run / size codes up to the end of block (T.81 F.2.2).
#[inline(always)]
fn sequential(r: &mut BitReader, dc: &Decoder, ac: &Decoder, pred: &mut i32, blk: &mut [i16; 64]) -> Result<(), Bad> {
    r.ensure();
    let size = dc.decode(r)? as u32;
    if size > 11 {
        return Err(Bad);
    }
    *pred = pred.wrapping_add(r.signed(size));
    blk[0] = *pred as i16;
    let mut k = 1usize;
    while k < 64 {
        r.ensure();
        let rs = ac.decode(r)?;
        let (run, size) = ((rs >> 4) as usize, (rs & 15) as u32);
        if size == 0 {
            if run != 15 {
                break;
            }
            k += 16;
            continue;
        }
        k += run;
        if k > 63 || size > 10 {
            return Err(Bad);
        }
        blk[k] = r.signed(size) as i16;
        k += 1;
    }
    Ok(())
}

/// First DC scan of a progressive picture (T.81 G.1.2.1).
#[inline(always)]
fn dc_first(r: &mut BitReader, dc: &Decoder, al: u32, pred: &mut i32, blk: &mut [i16; 64]) -> Result<(), Bad> {
    r.ensure();
    let size = dc.decode(r)? as u32;
    if size > 11 {
        return Err(Bad);
    }
    *pred = pred.wrapping_add(r.signed(size));
    blk[0] = (*pred << al) as i16;
    Ok(())
}

/// First AC scan of a band (T.81 G.1.2.2): coefficients and end-of-band runs.
#[inline(always)]
fn ac_first(r: &mut BitReader, ac: &Decoder, s: &Scan, eobrun: &mut u32, blk: &mut [i16; 64]) -> Result<(), Bad> {
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let mut k = s.ss;
    while k <= s.se {
        r.ensure();
        let rs = ac.decode(r)?;
        let (run, size) = ((rs >> 4) as u32, (rs & 15) as u32);
        if size == 0 {
            if run < 15 {
                // End of band, for this block and `eobrun` more.
                *eobrun = (1u32 << run) + r.bits(run) - 1;
                break;
            }
            k += 16;
            continue;
        }
        k += run as usize;
        if k > s.se || size > 10 {
            return Err(Bad);
        }
        blk[k] = (r.signed(size) << s.al) as i16;
        k += 1;
    }
    Ok(())
}

/// A refinement AC scan (T.81 G.1.2.3): one more bit for every coefficient
/// already non-zero, plus coefficients that become +-1 at this bit. Follows
/// libjpeg's decode_mcu_AC_refine.
#[inline(always)]
fn ac_refine(r: &mut BitReader, ac: &Decoder, s: &Scan, eobrun: &mut u32, blk: &mut [i16; 64]) -> Result<(), Bad> {
    let p1: i16 = 1 << s.al;
    let m1: i16 = -1 << s.al;
    let refine = |c: i16, r: &mut BitReader| -> i16 {
        if r.bit() && (c & p1) == 0 {
            if c >= 0 {
                c.wrapping_add(p1)
            } else {
                c.wrapping_add(m1)
            }
        } else {
            c
        }
    };
    let mut k = s.ss;
    if *eobrun == 0 {
        while k <= s.se {
            r.ensure();
            let rs = ac.decode(r)?;
            let mut run = (rs >> 4) as i32;
            let mut value = 0i16;
            if rs & 15 != 0 {
                value = if r.bit() { p1 } else { m1 };
            } else if run != 15 {
                *eobrun = (1u32 << run as u32) + r.bits(run as u32);
                break;
            }
            // Skip `run` zero coefficients, refining the non-zero ones passed.
            while k <= s.se {
                let c = blk[k];
                if c != 0 {
                    blk[k] = refine(c, r);
                } else {
                    run -= 1;
                    if run < 0 {
                        break;
                    }
                }
                k += 1;
            }
            if value != 0 {
                if k > s.se {
                    return Err(Bad);
                }
                blk[k] = value;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        // Inside an end-of-band run: only refine what is non-zero already.
        while k <= s.se {
            let c = blk[k];
            if c != 0 {
                blk[k] = refine(c, r);
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}
