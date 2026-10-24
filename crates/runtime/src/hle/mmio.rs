//! winmm's multimedia file I/O (mmio*), for reading: RIFF files (WAV) in memory
//! (FOURCC_MEM, a buffer the caller hands over) or from disk (read whole into a buffer of
//! their own). Every file is a view of guest memory, so buffered access (mmioGetInfo,
//! mmioAdvance) hands out the whole file as the buffer.

use super::{ApiResult, Ctx, Registry, Ret};

fn ok(n: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(n, v))
}

/// An open file: its bytes in guest memory, the position, whether the buffer is ours.
#[derive(Clone, Copy, Debug)]
pub struct File {
    base: u32,
    size: u32,
    pos: u32,
    owned: bool,
}

const FOURCC_MEM: u32 = u32::from_le_bytes(*b"MEM ");
const FOURCC_DOS: u32 = u32::from_le_bytes(*b"DOS ");
const FOURCC_RIFF: u32 = u32::from_le_bytes(*b"RIFF");
const FOURCC_LIST: u32 = u32::from_le_bytes(*b"LIST");
const MMIO_RWMODE: u32 = 0x3;
const MMIO_CREATE: u32 = 0x1000;
const MMIO_PARSE: u32 = 0x100;
const MMIO_EXIST: u32 = 0x4000;
const MMIO_FINDCHUNK: u32 = 0x10;
const MMIO_FINDRIFF: u32 = 0x20;
const MMIO_FINDLIST: u32 = 0x40;
const MMIO_TOUPPER: u32 = 0x10;
const MMIOERR_CANNOTOPEN: u32 = 257;
const MMIOERR_CHUNKNOTFOUND: u32 = 265;
const MMSYSERR_INVALHANDLE: u32 = 5;
/// MMIOINFO fields (32-bit layout).
const INFO_FCC: u32 = 4;
const INFO_ERROR: u32 = 12;
const INFO_CCH: u32 = 20;
const INFO_BUFFER: u32 = 24;
const INFO_NEXT: u32 = 28;
const INFO_END_READ: u32 = 32;
const INFO_END_WRITE: u32 = 36;
const INFO_BUF_OFFSET: u32 = 40;
const INFO_DISK_OFFSET: u32 = 44;
const INFO_HMMIO: u32 = 68;

fn open(c: &mut Ctx, name: Option<String>, info: u32, flags: u32) -> u32 {
    let fail = |c: &mut Ctx| {
        if info != 0 {
            let _ = c.host.write_u32(info + INFO_ERROR, MMIOERR_CANNOTOPEN);
        }
        0
    };
    let (fcc, buffer, cch) = if info != 0 {
        (c.host.read_u32(info + INFO_FCC).unwrap_or(0), c.host.read_u32(info + INFO_BUFFER).unwrap_or(0), c.host.read_u32(info + INFO_CCH).unwrap_or(0))
    } else {
        (0, 0, 0)
    };
    let file = if fcc == FOURCC_MEM || (buffer != 0 && name.is_none()) {
        if buffer == 0 {
            return fail(c);
        }
        File { base: buffer, size: cch, pos: 0, owned: false }
    } else {
        let Some(name) = name else { return fail(c) };
        let key = c.p.vpath(&name);
        if flags & MMIO_EXIST != 0 {
            return c.p.fs.is_file(&key) as u32;
        }
        if flags & MMIO_PARSE != 0 || flags & (MMIO_RWMODE | MMIO_CREATE) != 0 {
            c.p.log(format!("mmioOpen({name:?}, {flags:#x}): only reading is supported"));
            return fail(c);
        }
        let Some(bytes) = c.p.fs.read_all(&key) else { return fail(c) };
        let base = c.p.heap_alloc(c.host, 0, bytes.len().max(1) as u32, false);
        let _ = c.host.write(base, &bytes);
        File { base, size: bytes.len() as u32, pos: 0, owned: true }
    };
    let h = 0x6D00_0000 + c.p.mmio_next;
    c.p.mmio_next += 1;
    c.p.mmio.insert(h, file);
    if info != 0 {
        let _ = c.host.write_u32(info + INFO_ERROR, 0);
        let _ = c.host.write_u32(info + INFO_HMMIO, h);
    }
    h
}

/// The chunk header at the file's position: (id, size, form type for RIFF/LIST).
fn header(c: &Ctx, f: &File, end: u32) -> Option<(u32, u32, u32)> {
    if f.pos.checked_add(8)? > end {
        return None;
    }
    let id = c.host.read_u32(f.base + f.pos).ok()?;
    let size = c.host.read_u32(f.base + f.pos + 4).ok()?;
    let form = if id == FOURCC_RIFF || id == FOURCC_LIST { c.host.read_u32(f.base + f.pos + 8).unwrap_or(0) } else { 0 };
    Some((id, size, form))
}

/// mmioDescend: into the chunk at the position, or the next one matching `ck` (ckid; RIFF
/// or LIST with its form type) inside `parent`. MMCKINFO {ckid, cksize, fccType,
/// dwDataOffset, dwFlags}; after a RIFF or LIST chunk's header the position is past the
/// form type.
fn descend(c: &mut Ctx, h: u32, ck: u32, parent: u32, flags: u32) -> u32 {
    let Some(mut f) = c.p.mmio.get(&h).copied() else { return MMSYSERR_INVALHANDLE };
    let end = if parent != 0 {
        let off = c.host.read_u32(parent + 12).unwrap_or(0);
        off.saturating_add(c.host.read_u32(parent + 4).unwrap_or(0)).min(f.size)
    } else {
        f.size
    };
    let want_id = c.host.read_u32(ck).unwrap_or(0);
    let want_form = c.host.read_u32(ck + 8).unwrap_or(0);
    loop {
        let Some((id, size, form)) = header(c, &f, end) else { return MMIOERR_CHUNKNOTFOUND };
        let found = if flags & MMIO_FINDCHUNK != 0 {
            id == want_id
        } else if flags & MMIO_FINDRIFF != 0 {
            id == FOURCC_RIFF && form == want_form
        } else if flags & MMIO_FINDLIST != 0 {
            id == FOURCC_LIST && form == want_form
        } else {
            true
        };
        if found {
            let data = f.pos + 8;
            for (off, v) in [(0, id), (4, size), (8, form), (12, data), (16, 0)] {
                let _ = c.host.write_u32(ck + off, v);
            }
            f.pos = data + if id == FOURCC_RIFF || id == FOURCC_LIST { 4 } else { 0 };
            c.p.mmio.insert(h, f);
            return 0;
        }
        f.pos = f.pos.saturating_add(8).saturating_add(size).saturating_add(size & 1);
    }
}

/// mmioGetInfo's view of a file: all of it is the buffer.
fn write_info(c: &mut Ctx, h: u32, f: &File, info: u32) {
    let fcc = if f.owned { FOURCC_DOS } else { FOURCC_MEM };
    for (off, v) in [
        (INFO_FCC, fcc),
        (INFO_ERROR, 0),
        (INFO_CCH, f.size),
        (INFO_BUFFER, f.base),
        (INFO_NEXT, f.base + f.pos),
        (INFO_END_READ, f.base + f.size),
        (INFO_END_WRITE, f.base + f.size),
        (INFO_BUF_OFFSET, 0),
        (INFO_DISK_OFFSET, f.size),
        (INFO_HMMIO, h),
    ] {
        let _ = c.host.write_u32(info + off, v);
    }
}

/// The position the caller's pchNext stands for (mmioSetInfo, mmioAdvance).
fn take_info(c: &mut Ctx, h: u32, info: u32) -> Option<File> {
    let mut f = c.p.mmio.get(&h).copied()?;
    let next = c.host.read_u32(info + INFO_NEXT).unwrap_or(f.base + f.pos);
    f.pos = next.saturating_sub(f.base).min(f.size);
    c.p.mmio.insert(h, f);
    Some(f)
}

fn fourcc(s: &[u8], flags: u32) -> u32 {
    let mut b = [b' '; 4];
    for (i, &ch) in s.iter().take(4).enumerate() {
        b[i] = if flags & MMIO_TOUPPER != 0 { ch.to_ascii_uppercase() } else { ch };
    }
    u32::from_le_bytes(b)
}

pub fn register(r: &mut Registry) {
    let w = "winmm.dll";
    // (these names were stubs before: re-registering keeps their API ids)
    r.add(w, "mmioOpenA", |c| {
        let p = c.arg(0);
        let name = (p != 0).then(|| c.str_arg(0));
        let h = open(c, name, c.arg(1), c.arg(2));
        ok(3, h)
    });
    r.add(w, "mmioClose", |c| {
        let h = c.arg(0);
        match c.p.mmio.remove(&h) {
            Some(f) => {
                if f.owned {
                    c.p.heap_free(f.base);
                }
                ok(2, 0)
            }
            None => ok(2, MMSYSERR_INVALHANDLE),
        }
    });
    r.add(w, "mmioRead", |c| {
        let (h, buf, n) = (c.arg(0), c.arg(1), c.arg(2) as i32);
        let Some(mut f) = c.p.mmio.get(&h).copied() else { return ok(3, u32::MAX) };
        let n = (n.max(0) as u32).min(f.size - f.pos);
        let _ = c.host.copy_within(buf, f.base + f.pos, n);
        f.pos += n;
        c.p.mmio.insert(h, f);
        ok(3, n)
    });
    r.add(w, "mmioDescend", |c| {
        let r = descend(c, c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        ok(4, r)
    });
    r.add(w, "mmioAscend", |c| {
        let (h, ck) = (c.arg(0), c.arg(1));
        let Some(mut f) = c.p.mmio.get(&h).copied() else { return ok(3, MMSYSERR_INVALHANDLE) };
        let (size, data) = (c.host.read_u32(ck + 4).unwrap_or(0), c.host.read_u32(ck + 12).unwrap_or(0));
        f.pos = data.saturating_add(size).saturating_add(size & 1).min(f.size);
        c.p.mmio.insert(h, f);
        ok(3, 0)
    });
    r.add(w, "mmioStringToFOURCCA", |c| {
        let s = c.host.read_cstr(c.arg(0)).unwrap_or_default();
        ok(2, fourcc(&s, c.arg(1)))
    });
    r.add_on_demand(w, "mmioOpenW", |c| {
        let p = c.arg(0);
        let name = (p != 0).then(|| c.wstr_arg(0));
        let h = open(c, name, c.arg(1), c.arg(2));
        ok(3, h)
    });
    r.add_on_demand(w, "mmioStringToFOURCCW", |c| {
        let s: Vec<u8> = c.wstr_arg(0).chars().map(|ch| ch as u8).collect();
        ok(2, fourcc(&s, c.arg(1)))
    });
    r.add_on_demand(w, "mmioSeek", |c| {
        let (h, off, origin) = (c.arg(0), c.arg(1) as i32, c.arg(2));
        let Some(mut f) = c.p.mmio.get(&h).copied() else { return ok(3, u32::MAX) };
        let from = match origin {
            0 => 0i64,
            1 => f.pos as i64,
            _ => f.size as i64,
        };
        let to = from + off as i64;
        if to < 0 || to > f.size as i64 {
            return ok(3, u32::MAX);
        }
        f.pos = to as u32;
        c.p.mmio.insert(h, f);
        ok(3, f.pos)
    });
    r.add_on_demand(w, "mmioWrite", |_c| ok(3, u32::MAX));
    r.add_on_demand(w, "mmioGetInfo", |c| {
        let (h, info) = (c.arg(0), c.arg(1));
        let Some(f) = c.p.mmio.get(&h).copied() else { return ok(3, MMSYSERR_INVALHANDLE) };
        write_info(c, h, &f, info);
        ok(3, 0)
    });
    r.add_on_demand(w, "mmioSetInfo", |c| {
        let (h, info) = (c.arg(0), c.arg(1));
        let r = if take_info(c, h, info).is_some() { 0 } else { MMSYSERR_INVALHANDLE };
        ok(3, r)
    });
    // (the buffer is the whole file: advancing leaves it as it is, and at its end the
    // caller sees pchNext == pchEndRead)
    r.add_on_demand(w, "mmioAdvance", |c| {
        let (h, info) = (c.arg(0), c.arg(1));
        let Some(f) = take_info(c, h, info) else { return ok(3, MMSYSERR_INVALHANDLE) };
        write_info(c, h, &f, info);
        ok(3, 0)
    });
}
