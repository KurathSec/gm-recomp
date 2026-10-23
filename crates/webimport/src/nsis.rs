//! NSIS installers as GameMaker: Studio 1.4 exports them (NSIS 2, deflate, not solid): the
//! files the installer's script extracts, at the paths it extracts them to under the
//! install folder. Other NSIS flavours (solid, LZMA, bzip2, NSIS 3's Unicode) are not
//! read.

/// Opcodes of NSIS 2's script (exec.h).
const EW_CREATEDIR: u32 = 11;
const EW_EXTRACTFILE: u32 = 20;
/// Special characters in NSIS 2's ANSI strings (fileform.h).
const NS_SKIP_CODE: u8 = 252;
const NS_VAR_CODE: u8 = 253;
const NS_SHELL_CODE: u8 = 254;
const NS_LANG_CODE: u8 = 255;
/// Variables: $INSTDIR, $OUTDIR, $PLUGINSDIR.
const VAR_INSTDIR: u32 = 21;
const VAR_OUTDIR: u32 = 22;
const VAR_PLUGINSDIR: u32 = 26;

fn u32_at(d: &[u8], at: usize) -> Result<u32, String> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| "NSIS: truncated".to_string())
}

/// A block of the archive at `at`: a length (top bit: deflated) and the bytes.
fn block(d: &[u8], at: usize) -> Result<Vec<u8>, String> {
    let n = u32_at(d, at)?;
    let len = (n & 0x7FFF_FFFF) as usize;
    let raw = d.get(at + 4..at + 4 + len).ok_or("NSIS: a block runs past the end")?;
    if n & 0x8000_0000 == 0 {
        return Ok(raw.to_vec());
    }
    inflate(raw).map_err(|e| format!("NSIS: {e}"))
}

// ---- deflate as NSIS writes it --------------------------------------------------------------
// RFC 1951 (after zlib's puff.c), except that NSIS's stored blocks have no NLEN after their
// length, so a standard inflater rejects them.

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u32,
}

impl Bits<'_> {
    fn need(&mut self, n: u32) -> Result<u32, String> {
        let mut v = self.buf;
        while self.cnt < n {
            let b = *self.d.get(self.pos).ok_or("deflate data ends early")? as u32;
            self.pos += 1;
            v |= b << self.cnt;
            self.cnt += 8;
        }
        self.buf = if n == 32 { 0 } else { v >> n };
        self.cnt -= n;
        Ok(if n == 32 { v } else { v & ((1 << n) - 1) })
    }
}

struct Huff {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Huff {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[l as usize] += 1;
        }
        let mut offs = [0u16; 16];
        for k in 1..15 {
            offs[k + 1] = offs[k] + count[k];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        count[0] = 0;
        Huff { count, symbol }
    }

    fn decode(&self, b: &mut Bits) -> Result<u16, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.need(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return Ok(self.symbol[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("bad deflate code".into())
    }
}

const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Deflate64 (zip method 9): length code 285 takes 16 extra bits from 3, and distance
/// codes 30 and 31 reach 64 KB back.
fn codes(b: &mut Bits, out: &mut Vec<u8>, lens: &Huff, dists: &Huff, d64: bool) -> Result<(), String> {
    loop {
        let sym = lens.decode(b)? as usize;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let k = sym - 257;
            if k >= 29 {
                return Err("bad length code".into());
            }
            let len = if d64 && k == 28 { 3 + b.need(16)? as usize } else { LBASE[k] as usize + b.need(LEXT[k] as u32)? as usize };
            let d = dists.decode(b)? as usize;
            let dist = match d {
                0..30 => DBASE[d] as usize + b.need(DEXT[d] as u32)? as usize,
                30 | 31 if d64 => [32769, 49153][d - 30] + b.need(14)? as usize,
                _ => return Err("bad distance code".into()),
            };
            if dist > out.len() {
                return Err("distance too far back".into());
            }
            let from = out.len() - dist;
            for i in 0..len {
                let c = out[from + i];
                out.push(c);
            }
        }
    }
}

fn inflate(d: &[u8]) -> Result<Vec<u8>, String> {
    inflate_with(d, true, false)
}

/// A deflate stream as NSIS writes it (`nsis`: stored blocks without NLEN), or Deflate64.
pub(crate) fn inflate_with(d: &[u8], nsis: bool, d64: bool) -> Result<Vec<u8>, String> {
    let mut b = Bits { d, pos: 0, buf: 0, cnt: 0 };
    let mut out = Vec::new();
    loop {
        let last = b.need(1)?;
        match b.need(2)? {
            0 => {
                // stored: aligned length, and (NSIS) no NLEN
                b.buf = 0;
                b.cnt = 0;
                let len = b.need(16)? as usize;
                if !nsis {
                    b.need(16)?;
                }
                let src = d.get(b.pos..b.pos + len).ok_or("stored block runs past the end")?;
                out.extend_from_slice(src);
                b.pos += len;
            }
            1 => {
                let mut l = [0u8; 288];
                l[..144].fill(8);
                l[144..256].fill(9);
                l[256..280].fill(7);
                l[280..].fill(8);
                codes(&mut b, &mut out, &Huff::new(&l), &Huff::new(&[5u8; 32]), d64)?;
            }
            2 => {
                let nlen = b.need(5)? as usize + 257;
                let ndist = b.need(5)? as usize + 1;
                let ncode = b.need(4)? as usize + 4;
                const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut cl = [0u8; 19];
                for &o in ORDER.iter().take(ncode) {
                    cl[o] = b.need(3)? as u8;
                }
                let lencode = Huff::new(&cl);
                let mut lengths = vec![0u8; nlen + ndist];
                let mut i = 0;
                while i < nlen + ndist {
                    let sym = lencode.decode(&mut b)?;
                    let (val, rep) = match sym {
                        0..=15 => (sym as u8, 1),
                        16 => (*lengths.get(i.wrapping_sub(1)).ok_or("repeat with nothing before")?, 3 + b.need(2)? as usize),
                        17 => (0, 3 + b.need(3)? as usize),
                        _ => (0, 11 + b.need(7)? as usize),
                    };
                    if i + rep > nlen + ndist {
                        return Err("too many code lengths".into());
                    }
                    lengths[i..i + rep].fill(val);
                    i += rep;
                }
                codes(&mut b, &mut out, &Huff::new(&lengths[..nlen]), &Huff::new(&lengths[nlen..]), d64)?;
            }
            _ => return Err("bad deflate block type".into()),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// A string of the script: install-folder variables become the folder (""), the plugins
/// folder "$PLUGINSDIR", other variables and shell folders nothing.
fn string(strings: &[u8], at: usize) -> String {
    let mut out = Vec::new();
    let mut i = at;
    while let Some(&b) = strings.get(i) {
        i += 1;
        match b {
            0 => break,
            NS_VAR_CODE | NS_SHELL_CODE | NS_LANG_CODE => {
                let (b1, b2) = (*strings.get(i).unwrap_or(&0) as u32, *strings.get(i + 1).unwrap_or(&0) as u32);
                i += 2;
                if b == NS_VAR_CODE {
                    match (b1 & 0x7F) | (b2 & 0x7F) << 7 {
                        VAR_INSTDIR | VAR_OUTDIR => {}
                        VAR_PLUGINSDIR => out.extend_from_slice(b"$PLUGINSDIR"),
                        _ => {}
                    }
                }
            }
            NS_SKIP_CODE => {
                if let Some(&c) = strings.get(i) {
                    out.push(c);
                    i += 1;
                }
            }
            _ => out.push(b),
        }
    }
    crate::decode_name(&out, false)
}

/// The installer's files (path '/' separated, contents), or why they cannot be had.
pub fn extract(exe: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    // the first header: on a 512-byte boundary, after the stub
    let fh = (0..exe.len().saturating_sub(28))
        .step_by(512)
        .find(|&p| u32_at(exe, p + 4).ok() == Some(0xDEAD_BEEF) && &exe[p + 8..p + 20] == b"NullsoftInst")
        .ok_or("NSIS: no installer header")?;
    let header_len = u32_at(exe, fh + 20)? as usize;
    let data = &exe[fh + 28..];
    let first = u32_at(data, 0)?;
    if (first & 0x7FFF_FFFF) as usize + 4 > data.len() {
        return Err("NSIS: a solid installer, which cannot be read yet".into());
    }
    let header = block(data, 0)?;
    if header.len() != header_len {
        return Err("NSIS: an installer of another kind (not deflate, or solid), which cannot be read yet".into());
    }
    let files_at = 4 + (first & 0x7FFF_FFFF) as usize;
    // flags, then 8 blocks {offset, count}: pages, sections, entries, strings, ...
    let (entries_at, entries_n) = (u32_at(&header, 4 + 2 * 8)? as usize, u32_at(&header, 4 + 2 * 8 + 4)? as usize);
    let strings_at = u32_at(&header, 4 + 3 * 8)? as usize;
    let strings = header.get(strings_at..).ok_or("NSIS: no strings")?;
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut outdir = String::new();
    for k in 0..entries_n {
        // {which, parms[6]}
        let e = entries_at + k * 28;
        let which = u32_at(&header, e)?;
        let parm = |n: usize| u32_at(&header, e + 4 + 4 * n);
        match which {
            EW_CREATEDIR if parm(1)? != 0 => outdir = string(strings, parm(0)? as usize).replace('\\', "/").trim_matches('/').to_string(),
            EW_EXTRACTFILE => {
                let name = string(strings, parm(1)? as usize).replace('\\', "/");
                let path = if outdir.is_empty() { name } else { format!("{outdir}/{name}") };
                let path = path.trim_start_matches('/').to_string();
                if path.starts_with("$PLUGINSDIR") || path.is_empty() {
                    continue;
                }
                let bytes = block(data, files_at + parm(2)? as usize)?;
                out.retain(|(p, _)| !p.eq_ignore_ascii_case(&path));
                out.push((path, bytes));
            }
            _ => {}
        }
    }
    if out.is_empty() {
        return Err("NSIS: the installer extracts no files that could be read".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    /// Compressed blocks decode as zlib's (NSIS differs only in stored blocks).
    #[test]
    fn inflates_compressed_blocks() {
        let text: Vec<u8> = (0..200_000u32).map(|i| b"GameMaker data.win options.ini "[(i % 31) as usize] ^ ((i / 997) as u8 & 3)).collect();
        for level in [1, 6, 9] {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
            e.write_all(&text).unwrap();
            let packed = e.finish().unwrap();
            assert_eq!(super::inflate(&packed).unwrap(), text, "level {level}");
        }
    }

    /// A stored block as NSIS writes it: the length, no NLEN.
    #[test]
    fn stored_block_without_nlen() {
        let mut d = vec![0b001u8]; // last block, type 0 (stored)
        d.extend_from_slice(&5u16.to_le_bytes());
        d.extend_from_slice(b"hello");
        assert_eq!(super::inflate(&d).unwrap(), b"hello");
    }
}
