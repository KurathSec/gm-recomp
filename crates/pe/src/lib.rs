//! Minimal PE32 (i386) parser: headers, sections, imports (incl. delay-load),
//! exports, base relocations, resources, TLS and overlay. Everything borrows
//! from the file bytes; addresses are RVAs unless stated otherwise.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Truncated(&'static str),
    BadMagic(&'static str),
    Unsupported(String),
    BadRva(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated(what) => write!(f, "truncated {what}"),
            Error::BadMagic(what) => write!(f, "bad {what} magic"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::BadRva(rva) => write!(f, "rva {rva:#x} is not mapped by any section"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub const IMAGE_FILE_RELOCS_STRIPPED: u16 = 0x0001;
pub const IMAGE_FILE_DLL: u16 = 0x2000;
pub const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
pub const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
pub const IMAGE_SCN_CNT_CODE: u32 = 0x0000_0020;

pub const DIR_EXPORT: usize = 0;
pub const DIR_IMPORT: usize = 1;
pub const DIR_RESOURCE: usize = 2;
pub const DIR_BASERELOC: usize = 5;
pub const DIR_TLS: usize = 9;
pub const DIR_IAT: usize = 12;
pub const DIR_DELAY_IMPORT: usize = 13;
pub const DIR_CLR: usize = 14;

pub const RT_RCDATA: u32 = 10;

fn u16_at(d: &[u8], off: usize) -> Option<u16> {
    d.get(off..off + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    d.get(off..off + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw_offset: u32,
    pub raw_size: u32,
    pub characteristics: u32,
}

impl Section {
    pub fn is_executable(&self) -> bool {
        self.characteristics & (IMAGE_SCN_MEM_EXECUTE | IMAGE_SCN_CNT_CODE) != 0
    }

    pub fn is_writable(&self) -> bool {
        self.characteristics & IMAGE_SCN_MEM_WRITE != 0
    }

    /// Size the section occupies once mapped (raw data may be shorter: the rest is zero-filled).
    pub fn mapped_size(&self) -> u32 {
        if self.virtual_size == 0 { self.raw_size } else { self.virtual_size }
    }

    pub fn contains_rva(&self, rva: u32) -> bool {
        rva >= self.virtual_address && rva - self.virtual_address < self.mapped_size().max(self.raw_size)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DataDir {
    pub rva: u32,
    pub size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportName {
    Name { hint: u16, name: String },
    Ordinal(u16),
}

impl fmt::Display for ImportName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportName::Name { name, .. } => f.write_str(name),
            ImportName::Ordinal(o) => write!(f, "#{o}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Import {
    pub name: ImportName,
    /// RVA of the IAT slot the loader patches with the resolved address.
    pub iat_rva: u32,
}

#[derive(Debug, Clone)]
pub struct ImportDll {
    pub dll: String,
    pub delay_load: bool,
    pub entries: Vec<Import>,
}

#[derive(Debug, Clone)]
pub struct Export {
    pub ordinal: u16,
    pub name: Option<String>,
    pub rva: u32,
    /// Set when the export forwards to another DLL ("DLL.Func").
    pub forwarder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResId {
    Id(u32),
    Name(String),
}

impl fmt::Display for ResId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResId::Id(i) => write!(f, "{i}"),
            ResId::Name(n) => f.write_str(n),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resource {
    pub kind: ResId,
    pub name: ResId,
    pub lang: u32,
    pub rva: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Tls {
    pub start_va: u32,
    pub end_va: u32,
    pub index_va: u32,
    pub callbacks_va: u32,
    pub zero_fill: u32,
    /// Callback VAs read from the (file-backed) callback array.
    pub callbacks: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct Pe<'a> {
    pub data: &'a [u8],
    pub machine: u16,
    pub characteristics: u16,
    pub timestamp: u32,
    /// The preferred base in 32-bit address space: a PE32+ (64-bit) image's lies above
    /// 4 GB and is given as 0 here, so its VAs below (TLS, callbacks) read as RVAs and the
    /// loader always rebases it (see `image_base64`).
    pub image_base: u32,
    /// PE32+: a 64-bit image (x86-64).
    pub pe64: bool,
    /// The preferred base as the optional header gives it.
    pub image_base64: u64,
    pub entry_rva: u32,
    pub section_alignment: u32,
    pub file_alignment: u32,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub subsystem: u16,
    pub dll_characteristics: u16,
    pub stack_reserve: u32,
    pub stack_commit: u32,
    pub heap_reserve: u32,
    pub heap_commit: u32,
    pub sections: Vec<Section>,
    pub dirs: Vec<DataDir>,
}

impl<'a> Pe<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        if data.get(0..2) != Some(b"MZ") {
            return Err(Error::BadMagic("MZ"));
        }
        let pe_off = u32_at(data, 0x3c).ok_or(Error::Truncated("dos header"))? as usize;
        if data.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
            return Err(Error::BadMagic("PE"));
        }
        let coff = pe_off + 4;
        let rd16 = |o: usize, what| u16_at(data, o).ok_or(Error::Truncated(what));
        let rd32 = |o: usize, what| u32_at(data, o).ok_or(Error::Truncated(what));
        let machine = rd16(coff, "coff")?;
        let nsec = rd16(coff + 2, "coff")? as usize;
        let timestamp = rd32(coff + 4, "coff")?;
        let opt_size = rd16(coff + 16, "coff")? as usize;
        let characteristics = rd16(coff + 18, "coff")?;
        let opt = coff + 20;
        let magic = rd16(opt, "optional header")?;
        let pe64 = match (magic, machine) {
            (0x10b, 0x14c) => false,
            (0x20b, 0x8664) => true,
            (0x10b | 0x20b, _) => return Err(Error::Unsupported(format!("machine {machine:#x} (only i386 and x86-64 are supported)"))),
            _ => return Err(Error::Unsupported(format!("optional header magic {magic:#x}"))),
        };
        // PE32+ widens ImageBase and the four stack/heap sizes to 64 bits (and drops
        // BaseOfData): the data directories start 16 bytes further on
        let w = if pe64 { 16 } else { 0 };
        let rd64 = |o: usize, what| Ok::<u64, Error>(rd32(o, what)? as u64 | (rd32(o + 4, what)? as u64) << 32);
        let ndirs = (rd32(opt + 92 + w, "optional header")? as usize).min(16);
        let mut dirs = Vec::with_capacity(16);
        for i in 0..16 {
            dirs.push(if i < ndirs {
                DataDir { rva: rd32(opt + 96 + w + i * 8, "data dirs")?, size: rd32(opt + 100 + w + i * 8, "data dirs")? }
            } else {
                DataDir::default()
            });
        }
        let image_base64 = if pe64 { rd64(opt + 24, "optional header")? } else { rd32(opt + 28, "optional header")? as u64 };
        let size = |i: usize| -> Result<u32> { if pe64 { Ok(rd64(opt + 72 + 8 * i, "optional header")?.min(u32::MAX as u64) as u32) } else { rd32(opt + 72 + 4 * i, "optional header") } };
        let sec_tab = opt + opt_size;
        let mut sections = Vec::with_capacity(nsec);
        for i in 0..nsec {
            let s = sec_tab + i * 40;
            let raw_name = data.get(s..s + 8).ok_or(Error::Truncated("section table"))?;
            let name_len = raw_name.iter().position(|&b| b == 0).unwrap_or(8);
            sections.push(Section {
                name: String::from_utf8_lossy(&raw_name[..name_len]).into_owned(),
                virtual_size: rd32(s + 8, "section table")?,
                virtual_address: rd32(s + 12, "section table")?,
                raw_size: rd32(s + 16, "section table")?,
                raw_offset: rd32(s + 20, "section table")?,
                characteristics: rd32(s + 36, "section table")?,
            });
        }
        Ok(Pe {
            data,
            machine,
            characteristics,
            timestamp,
            image_base: if pe64 { 0 } else { image_base64 as u32 },
            pe64,
            image_base64,
            entry_rva: rd32(opt + 16, "optional header")?,
            section_alignment: rd32(opt + 32, "optional header")?,
            file_alignment: rd32(opt + 36, "optional header")?,
            size_of_image: rd32(opt + 56, "optional header")?,
            size_of_headers: rd32(opt + 60, "optional header")?,
            subsystem: rd16(opt + 68, "optional header")?,
            dll_characteristics: rd16(opt + 70, "optional header")?,
            stack_reserve: size(0)?,
            stack_commit: size(1)?,
            heap_reserve: size(2)?,
            heap_commit: size(3)?,
            sections,
            dirs,
        })
    }

    pub fn is_dll(&self) -> bool {
        self.characteristics & IMAGE_FILE_DLL != 0
    }

    pub fn relocs_stripped(&self) -> bool {
        self.characteristics & IMAGE_FILE_RELOCS_STRIPPED != 0
    }

    pub fn dir(&self, idx: usize) -> DataDir {
        self.dirs[idx]
    }

    pub fn section_of(&self, rva: u32) -> Option<&Section> {
        self.sections.iter().find(|s| s.contains_rva(rva))
    }

    /// File offset backing `rva`, or None when the RVA is zero-fill / unmapped.
    pub fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        if rva < self.size_of_headers {
            return Some(rva as usize);
        }
        let s = self.section_of(rva)?;
        let delta = rva - s.virtual_address;
        if delta >= s.raw_size {
            return None;
        }
        Some((s.raw_offset + delta) as usize)
    }

    pub fn bytes_at(&self, rva: u32, len: usize) -> Result<&'a [u8]> {
        let off = self.rva_to_offset(rva).ok_or(Error::BadRva(rva))?;
        self.data.get(off..off + len).ok_or(Error::Truncated("rva range"))
    }

    pub fn u16_at_rva(&self, rva: u32) -> Result<u16> {
        let b = self.bytes_at(rva, 2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32_at_rva(&self, rva: u32) -> Result<u32> {
        let b = self.bytes_at(rva, 4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn cstr_at_rva(&self, rva: u32) -> Result<String> {
        let off = self.rva_to_offset(rva).ok_or(Error::BadRva(rva))?;
        let tail = &self.data[off..];
        let len = tail.iter().position(|&b| b == 0).ok_or(Error::Truncated("c string"))?;
        Ok(String::from_utf8_lossy(&tail[..len]).into_owned())
    }

    pub fn va_to_rva(&self, va: u32) -> Option<u32> {
        va.checked_sub(self.image_base).filter(|&r| r < self.size_of_image)
    }

    /// Reads a thunk table (ILT/INT) and pairs every entry with its IAT slot.
    fn read_thunks(&self, lookup_rva: u32, iat_rva: u32, rva_based_names: bool) -> Result<Vec<Import>> {
        let mut out = Vec::new();
        // PE32+ thunks are 8 bytes, the ordinal flag in bit 63 (names are RVAs below 4 GB)
        let size = self.thunk_size();
        for i in 0.. {
            let (thunk, ordinal) = if self.pe64 {
                let hi = self.u32_at_rva(lookup_rva + i * 8 + 4)?;
                (self.u32_at_rva(lookup_rva + i * 8)?, hi & 0x8000_0000 != 0)
            } else {
                let t = self.u32_at_rva(lookup_rva + i * 4)?;
                (t, t & 0x8000_0000 != 0)
            };
            if thunk == 0 && !ordinal {
                break;
            }
            let name = if ordinal {
                ImportName::Ordinal(thunk as u16)
            } else {
                let mut hn = thunk;
                if !rva_based_names {
                    hn = self.va_to_rva(hn).ok_or(Error::BadRva(hn))?;
                }
                ImportName::Name { hint: self.u16_at_rva(hn)?, name: self.cstr_at_rva(hn + 2)? }
            };
            out.push(Import { name, iat_rva: iat_rva + i * size });
        }
        Ok(out)
    }

    /// Bytes per import thunk and IAT slot: 8 in a PE32+ image.
    pub fn thunk_size(&self) -> u32 {
        if self.pe64 { 8 } else { 4 }
    }

    pub fn imports(&self) -> Result<Vec<ImportDll>> {
        let mut out = Vec::new();
        let dir = self.dir(DIR_IMPORT);
        if dir.rva != 0 {
            let mut desc = dir.rva;
            loop {
                let ilt = self.u32_at_rva(desc)?;
                let name_rva = self.u32_at_rva(desc + 12)?;
                let iat = self.u32_at_rva(desc + 16)?;
                if name_rva == 0 && iat == 0 {
                    break;
                }
                // Borland linkers leave the ILT empty; the IAT then still holds the names.
                let lookup = if ilt != 0 { ilt } else { iat };
                out.push(ImportDll {
                    dll: self.cstr_at_rva(name_rva)?,
                    delay_load: false,
                    entries: self.read_thunks(lookup, iat, true)?,
                });
                desc += 20;
            }
        }
        // Delay imports are the exe's own business (its helper loads them when first
        // called): the loader never reads them, and a packer (UPX) leaves the original's
        // table pointing at what is packed data now. A table that does not parse ends there.
        let dir = self.dir(DIR_DELAY_IMPORT);
        if dir.rva != 0 {
            let mut desc = dir.rva;
            while let Ok(Some(dll)) = self.delay_import_at(desc) {
                out.push(dll);
                desc += 32;
            }
        }
        Ok(out)
    }

    /// The delay import descriptor at `desc`, None at the table's end.
    fn delay_import_at(&self, desc: u32) -> Result<Option<ImportDll>> {
        let attrs = self.u32_at_rva(desc)?;
        let name = self.u32_at_rva(desc + 4)?;
        if name == 0 {
            return Ok(None);
        }
        // Bit 0 of attributes: fields are RVAs (VC7+). Older Borland/VC6 tables hold VAs.
        let rva_based = attrs & 1 != 0;
        let fix = |v: u32| if rva_based { Some(v) } else { self.va_to_rva(v) };
        let name_rva = fix(name).ok_or(Error::BadRva(name))?;
        let iat_raw = self.u32_at_rva(desc + 12)?;
        let int_raw = self.u32_at_rva(desc + 16)?;
        let iat = fix(iat_raw).ok_or(Error::BadRva(iat_raw))?;
        let int = fix(int_raw).ok_or(Error::BadRva(int_raw))?;
        Ok(Some(ImportDll { dll: self.cstr_at_rva(name_rva)?, delay_load: true, entries: self.read_thunks(int, iat, rva_based)? }))
    }

    pub fn exports(&self) -> Result<Vec<Export>> {
        let dir = self.dir(DIR_EXPORT);
        if dir.rva == 0 {
            return Ok(Vec::new());
        }
        let base = self.u32_at_rva(dir.rva + 16)?;
        let nfuncs = self.u32_at_rva(dir.rva + 20)?;
        let nnames = self.u32_at_rva(dir.rva + 24)?;
        let funcs = self.u32_at_rva(dir.rva + 28)?;
        let names = self.u32_at_rva(dir.rva + 32)?;
        let ords = self.u32_at_rva(dir.rva + 36)?;
        let mut named = vec![None; nfuncs as usize];
        for i in 0..nnames {
            let idx = self.u16_at_rva(ords + i * 2)? as usize;
            if let Some(slot) = named.get_mut(idx) {
                *slot = Some(self.cstr_at_rva(self.u32_at_rva(names + i * 4)?)?);
            }
        }
        let mut out = Vec::new();
        for (i, name) in named.into_iter().enumerate() {
            let rva = self.u32_at_rva(funcs + i as u32 * 4)?;
            if rva == 0 {
                continue;
            }
            let forwarder = if rva >= dir.rva && rva < dir.rva + dir.size { Some(self.cstr_at_rva(rva)?) } else { None };
            out.push(Export { ordinal: (base + i as u32) as u16, name, rva, forwarder });
        }
        Ok(out)
    }

    /// RVAs of every IMAGE_REL_BASED_HIGHLOW fixup (PE32) or IMAGE_REL_BASED_DIR64 fixup
    /// (PE32+): 32- or 64-bit absolute addresses, as `pe64` says.
    pub fn relocations(&self) -> Result<Vec<u32>> {
        let dir = self.dir(DIR_BASERELOC);
        let mut out = Vec::new();
        if dir.rva == 0 {
            return Ok(out);
        }
        let mut pos = 0u32;
        while pos + 8 <= dir.size {
            let page = self.u32_at_rva(dir.rva + pos)?;
            let block = self.u32_at_rva(dir.rva + pos + 4)?;
            if block < 8 {
                break;
            }
            for i in 0..(block - 8) / 2 {
                let e = self.u16_at_rva(dir.rva + pos + 8 + i * 2)?;
                match e >> 12 {
                    0 => {}
                    3 if !self.pe64 => out.push(page + (e & 0xfff) as u32),
                    10 if self.pe64 => out.push(page + (e & 0xfff) as u32),
                    k => return Err(Error::Unsupported(format!("relocation type {k}"))),
                }
            }
            pos += block;
        }
        Ok(out)
    }

    fn res_id(&self, root: u32, raw: u32) -> Result<ResId> {
        if raw & 0x8000_0000 != 0 {
            let at = root + (raw & 0x7fff_ffff);
            let len = self.u16_at_rva(at)? as usize;
            let bytes = self.bytes_at(at + 2, len * 2)?;
            let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            Ok(ResId::Name(String::from_utf16_lossy(&units)))
        } else {
            Ok(ResId::Id(raw))
        }
    }

    fn res_dir_entries(&self, root: u32, dir: u32) -> Result<Vec<(u32, u32)>> {
        let named = self.u16_at_rva(root + dir + 12)? as u32;
        let ids = self.u16_at_rva(root + dir + 14)? as u32;
        (0..named + ids)
            .map(|i| Ok((self.u32_at_rva(root + dir + 16 + i * 8)?, self.u32_at_rva(root + dir + 20 + i * 8)?)))
            .collect()
    }

    /// Flattens the standard three-level (type / name / language) resource tree.
    pub fn resources(&self) -> Result<Vec<Resource>> {
        let root = self.dir(DIR_RESOURCE).rva;
        let mut out = Vec::new();
        if root == 0 {
            return Ok(out);
        }
        for (kind_raw, kind_ptr) in self.res_dir_entries(root, 0)? {
            if kind_ptr & 0x8000_0000 == 0 {
                continue;
            }
            let kind = self.res_id(root, kind_raw)?;
            for (name_raw, name_ptr) in self.res_dir_entries(root, kind_ptr & 0x7fff_ffff)? {
                if name_ptr & 0x8000_0000 == 0 {
                    continue;
                }
                let name = self.res_id(root, name_raw)?;
                for (lang, leaf) in self.res_dir_entries(root, name_ptr & 0x7fff_ffff)? {
                    if leaf & 0x8000_0000 != 0 {
                        continue;
                    }
                    out.push(Resource {
                        kind: kind.clone(),
                        name: name.clone(),
                        lang,
                        rva: self.u32_at_rva(root + leaf)?,
                        size: self.u32_at_rva(root + leaf + 4)?,
                    });
                }
            }
        }
        Ok(out)
    }

    pub fn resource_data(&self, r: &Resource) -> Result<&'a [u8]> {
        self.bytes_at(r.rva, r.size as usize)
    }

    pub fn tls(&self) -> Result<Option<Tls>> {
        let dir = self.dir(DIR_TLS);
        if dir.rva == 0 {
            return Ok(None);
        }
        // PE32+: four 64-bit VAs, given here as RVAs (see `image_base`)
        let ptr = |i: u32| -> Result<u32> {
            if self.pe64 {
                let va = self.u32_at_rva(dir.rva + 8 * i)? as u64 | (self.u32_at_rva(dir.rva + 8 * i + 4)? as u64) << 32;
                Ok(if va == 0 { 0 } else { va.wrapping_sub(self.image_base64) as u32 })
            } else {
                self.u32_at_rva(dir.rva + 4 * i)
            }
        };
        let mut t = Tls {
            start_va: ptr(0)?,
            end_va: ptr(1)?,
            index_va: ptr(2)?,
            callbacks_va: ptr(3)?,
            zero_fill: self.u32_at_rva(dir.rva + if self.pe64 { 32 } else { 16 })?,
            callbacks: Vec::new(),
        };
        if let Some(mut rva) = self.va_to_rva(t.callbacks_va).filter(|_| t.callbacks_va != 0) {
            // The array may live in zero-fill memory that only the loader populates.
            let size = self.thunk_size();
            while let Ok(cb) = self.u32_at_rva(rva) {
                let cb = if self.pe64 {
                    let hi = self.u32_at_rva(rva + 4).unwrap_or(0);
                    if cb == 0 && hi == 0 {
                        break;
                    }
                    (cb as u64 | (hi as u64) << 32).wrapping_sub(self.image_base64) as u32
                } else {
                    if cb == 0 {
                        break;
                    }
                    cb
                };
                t.callbacks.push(cb);
                rva += size;
            }
        }
        Ok(Some(t))
    }

    /// File offset where data appended after the last section begins.
    pub fn overlay_offset(&self) -> usize {
        let end = self.sections.iter().map(|s| (s.raw_offset + s.raw_size) as usize).max().unwrap_or(0);
        end.max(self.size_of_headers as usize)
    }

    pub fn overlay(&self) -> &'a [u8] {
        self.data.get(self.overlay_offset()..).unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiny PE32 with one code section and a one-entry import table.
    fn tiny_pe() -> Vec<u8> {
        let mut d = vec![0u8; 0x400];
        d[0..2].copy_from_slice(b"MZ");
        d[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        d[0x80..0x84].copy_from_slice(b"PE\0\0");
        let coff = 0x84;
        d[coff..coff + 2].copy_from_slice(&0x14cu16.to_le_bytes());
        d[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        d[coff + 16..coff + 18].copy_from_slice(&224u16.to_le_bytes());
        d[coff + 18..coff + 20].copy_from_slice(&0x0102u16.to_le_bytes());
        let opt = coff + 20;
        d[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        d[opt + 16..opt + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        d[opt + 28..opt + 32].copy_from_slice(&0x400000u32.to_le_bytes());
        d[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes());
        d[opt + 36..opt + 40].copy_from_slice(&0x200u32.to_le_bytes());
        d[opt + 56..opt + 60].copy_from_slice(&0x2000u32.to_le_bytes());
        d[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
        d[opt + 92..opt + 96].copy_from_slice(&16u32.to_le_bytes());
        // import dir at rva 0x1100
        d[opt + 96 + 8..opt + 96 + 12].copy_from_slice(&0x1100u32.to_le_bytes());
        d[opt + 96 + 12..opt + 96 + 16].copy_from_slice(&40u32.to_le_bytes());
        let sec = opt + 224;
        d[sec..sec + 5].copy_from_slice(b".text");
        d[sec + 8..sec + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        d[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        d[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes());
        d[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes());
        d[sec + 36..sec + 40].copy_from_slice(&(IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_EXECUTE).to_le_bytes());
        let at = |rva: u32| 0x200 + (rva - 0x1000) as usize;
        // descriptor: ILT 0x1140, name 0x1160, IAT 0x1150
        let desc = at(0x1100);
        d[desc..desc + 4].copy_from_slice(&0x1140u32.to_le_bytes());
        d[desc + 12..desc + 16].copy_from_slice(&0x1160u32.to_le_bytes());
        d[desc + 16..desc + 20].copy_from_slice(&0x1150u32.to_le_bytes());
        d[at(0x1140)..at(0x1140) + 4].copy_from_slice(&0x1170u32.to_le_bytes());
        d[at(0x1150)..at(0x1150) + 4].copy_from_slice(&0x1170u32.to_le_bytes());
        d[at(0x1160)..at(0x1160) + 12].copy_from_slice(b"kernel32.dll");
        d[at(0x1172)..at(0x1172) + 5].copy_from_slice(b"Sleep");
        d.extend_from_slice(b"OVERLAY");
        d
    }

    #[test]
    fn parses_headers_imports_and_overlay() {
        let bytes = tiny_pe();
        let pe = Pe::parse(&bytes).unwrap();
        assert_eq!(pe.image_base, 0x400000);
        assert_eq!(pe.sections.len(), 1);
        assert!(pe.sections[0].is_executable());
        let imps = pe.imports().unwrap();
        assert_eq!(imps.len(), 1);
        assert_eq!(imps[0].dll, "kernel32.dll");
        assert_eq!(imps[0].entries[0].name.to_string(), "Sleep");
        assert_eq!(imps[0].entries[0].iat_rva, 0x1150);
        assert_eq!(pe.overlay(), b"OVERLAY");
    }

    #[test]
    fn rejects_non_pe() {
        assert_eq!(Pe::parse(b"hello").unwrap_err(), Error::BadMagic("MZ"));
    }
}
