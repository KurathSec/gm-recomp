//! Importing a game in the browser from the archive it is stored as (zip, 7z or RAR):
//! what the native `importer` does, in memory. It unpacks the archive (names in UTF-8,
//! Shift-JIS or cp437; a single shared top folder stripped), unwraps an IExpress package (its
//! cabinet, MSZIP or LZX), picks the entry exe, identifies the runner and names the
//! system DLLs the game's modules need that it neither ships nor the runtime
//! emulates. The page fetches those, adds them under C/Windows/system32 and asks
//! again (they may need others).
//!
//! The page drives it through the exports at the end: `alloc`, `import`, `needs`,
//! `add_file`, `manifest` and `file_*`; text comes back through `out_ptr`/`out_len`.

use pe::{Pe, RT_RCDATA, ResId};
use serde_json::{Value, json};
use std::io::Read;

pub struct Game {
    pub title: String,
    pub source: String,
    /// Path ('/' separated) and contents.
    pub files: Vec<(String, Vec<u8>)>,
    /// The IExpress stub, the program it runs and its title.
    pub iexpress: Option<(String, String, String)>,
}

fn u16le(d: &[u8], o: usize) -> Result<u16, String> {
    d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or_else(|| "truncated zip".to_string())
}

fn u32le(d: &[u8], o: usize) -> Result<u32, String> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| "truncated zip".to_string())
}

fn u64le(d: &[u8], o: usize) -> Result<u64, String> {
    d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| "truncated zip".to_string())
}

/// A zip entry name: UTF-8, then cp932 (zips made on Japanese Windows store Shift-JIS
/// without the UTF-8 flag), then cp437, as the native importer decodes them.
pub fn decode_name(raw: &[u8], utf8: bool) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_owned();
    }
    if utf8 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let (s, _, had_errors) = encoding_rs::SHIFT_JIS.decode(raw);
    if !had_errors {
        return s.into_owned();
    }
    const HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";
    raw.iter().map(|&b| if b < 0x80 { b as char } else { HIGH.chars().nth((b - 0x80) as usize).unwrap_or('?') }).collect()
}

/// The path's parts, or None when it would leave the game's folder.
fn safe_parts(name: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            p if p.contains(':') => return None,
            p => out.push(p.to_owned()),
        }
    }
    Some(out)
}

/// The zip's files and folders: (path parts, is a folder, contents).
pub fn read_zip(z: &[u8]) -> Result<Vec<(Vec<String>, bool, Vec<u8>)>, String> {
    let lowest = z.len().saturating_sub(22 + 65535);
    let eocd = (lowest..=z.len().saturating_sub(22)).rev().find(|&i| u32le(z, i) == Ok(0x0605_4b50)).ok_or("not a zip archive")?;
    let mut count = u16le(z, eocd + 10)? as u64;
    let mut cd = u32le(z, eocd + 16)? as u64;
    if (count == 0xffff || cd == 0xffff_ffff) && eocd >= 20 && u32le(z, eocd - 20) == Ok(0x0706_4b50) {
        let z64 = u64le(z, eocd - 20 + 8)? as usize;
        if u32le(z, z64) == Ok(0x0606_4b50) {
            count = u64le(z, z64 + 32)?;
            cd = u64le(z, z64 + 48)?;
        }
    }
    let mut out = Vec::new();
    let mut p = cd as usize;
    for _ in 0..count {
        if u32le(z, p)? != 0x0201_4b50 {
            return Err("bad zip central directory".into());
        }
        let flags = u16le(z, p + 8)?;
        let method = u16le(z, p + 10)?;
        let mut csize = u32le(z, p + 20)? as u64;
        let mut size = u32le(z, p + 24)? as u64;
        let (n, e, c) = (u16le(z, p + 28)? as usize, u16le(z, p + 30)? as usize, u16le(z, p + 32)? as usize);
        let mut local = u32le(z, p + 42)? as u64;
        let raw_name = z.get(p + 46..p + 46 + n).ok_or("truncated zip")?;
        // zip64 sizes and offset: in this order, only those that overflowed
        let mut x = p + 46 + n;
        while x + 4 <= p + 46 + n + e {
            let (id, len) = (u16le(z, x)?, u16le(z, x + 2)? as usize);
            if id == 1 {
                let mut q = x + 4;
                for field in [&mut size, &mut csize, &mut local] {
                    if *field == 0xffff_ffff {
                        *field = u64le(z, q)?;
                        q += 8;
                    }
                }
            }
            x += 4 + len;
        }
        p += 46 + n + e + c;
        let name = decode_name(raw_name, flags & 0x800 != 0);
        let is_dir = name.ends_with('/') || name.ends_with('\\');
        let parts = safe_parts(&name).ok_or_else(|| format!("unsafe zip entry name {name:?}"))?;
        if parts.is_empty() {
            continue;
        }
        if is_dir {
            out.push((parts, true, Vec::new()));
            continue;
        }
        if flags & 1 != 0 {
            return Err(format!("{name} is encrypted"));
        }
        let l = local as usize;
        if u32le(z, l)? != 0x0403_4b50 {
            return Err(format!("bad local header for {name}"));
        }
        let start = l + 30 + u16le(z, l + 26)? as usize + u16le(z, l + 28)? as usize;
        let raw = z.get(start..start + csize as usize).ok_or("truncated zip")?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut v = Vec::with_capacity(size as usize);
                flate2::read::DeflateDecoder::new(raw).read_to_end(&mut v).map_err(|e| format!("{name}: {e}"))?;
                v
            }
            m => return Err(format!("{name}: compression method {m} is not supported")),
        };
        out.push((parts, false, data));
    }
    Ok(out)
}

/// A 7z archive's files and folders, as `read_zip` gives them.
pub fn read_7z(data: &[u8]) -> Result<Vec<(Vec<String>, bool, Vec<u8>)>, String> {
    let mut reader = sevenz_rust2::ArchiveReader::new(std::io::Cursor::new(data), sevenz_rust2::Password::empty()).map_err(|e| format!("7z: {e}"))?;
    let mut out = Vec::new();
    let mut failed = None;
    reader
        .for_each_entries(|entry, r| {
            let name = entry.name().to_owned();
            let Some(parts) = safe_parts(&name).filter(|p| !p.is_empty()) else {
                failed = Some(format!("unsafe 7z entry name {name:?}"));
                return Ok(false);
            };
            if entry.is_directory() {
                out.push((parts, true, Vec::new()));
            } else {
                let mut v = Vec::with_capacity(entry.size() as usize);
                r.read_to_end(&mut v)?;
                out.push((parts, false, v));
            }
            Ok(true)
        })
        .map_err(|e| format!("7z: {e}"))?;
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// A RAR archive's files and folders (RAR 1.3 to 7, through `rars`), as `read_zip`
/// gives them.
pub fn read_rar(data: &[u8]) -> Result<Vec<(Vec<String>, bool, Vec<u8>)>, String> {
    use std::sync::{Arc, Mutex};
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let archive = rars::ArchiveReader::read(data).map_err(|e| format!("RAR: {e}"))?;
    let mut entries: Vec<(String, bool, Arc<Mutex<Vec<u8>>>)> = Vec::new();
    archive
        .extract_to(None, |meta| {
            let name = if meta.name_is_unicode { String::from_utf8_lossy(&meta.name).into_owned() } else { decode_name(&meta.name, false) };
            let buf = Arc::new(Mutex::new(Vec::new()));
            entries.push((name, meta.is_directory, buf.clone()));
            Ok(Box::new(Sink(buf)) as Box<dyn std::io::Write>)
        })
        .map_err(|e| format!("RAR: {e}"))?;
    let mut out = Vec::new();
    for (name, is_dir, buf) in entries {
        let parts = safe_parts(&name).filter(|p| !p.is_empty()).ok_or_else(|| format!("unsafe RAR entry name {name:?}"))?;
        let data = std::mem::take(&mut *buf.lock().unwrap());
        out.push((parts, is_dir, data));
    }
    Ok(out)
}

/// Unpacks an archive (zip, 7z or RAR, by content: renamed ones are common; or a bare exe)
/// into a game:
/// files with a single shared top folder stripped (its name is the title then), an
/// IExpress package unwrapped.
pub fn import(archive: &[u8], name: &str) -> Result<Game, String> {
    let entries = if archive.starts_with(b"7z\xbc\xaf\x27\x1c") {
        read_7z(archive)?
    } else if archive.starts_with(b"Rar!") {
        read_rar(archive)?
    } else if archive.starts_with(b"MZ") {
        // a self-extracting zip, else a game released as its exe alone (GameMaker 8 keeps
        // the game data inside)
        read_zip(archive).unwrap_or_else(|_| {
            let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
            vec![(vec![file.to_owned()], false, archive.to_vec())]
        })
    } else {
        read_zip(archive)?
    };
    let top = entries.first().map(|(p, _, _)| p[0].clone());
    let shared = top.filter(|t| entries.iter().all(|(p, is_dir, _)| &p[0] == t && (p.len() > 1 || *is_dir)));
    let mut files = Vec::new();
    for (parts, is_dir, data) in entries {
        let parts = if shared.is_some() { &parts[1..] } else { &parts[..] };
        if is_dir || parts.is_empty() {
            continue;
        }
        let path = parts.join("/");
        // (a later entry of the same name replaces an earlier one, as extracting does)
        files.retain(|(p, _): &(String, Vec<u8>)| !p.eq_ignore_ascii_case(&path));
        files.push((path, data));
    }
    let stem = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s).to_owned();
    let mut game = Game { title: shared.unwrap_or(stem), source: name.to_owned(), files, iexpress: None };
    // (the game's folder first: an IExpress package in a folder unwraps from there)
    game.reroot();
    game.iexpress = unwrap_iexpress(&mut game.files)?;
    game.files.sort_by(|a, b| a.0.cmp(&b.0));
    let (entry, _) = game.entry();
    if game.find(&entry).is_some_and(|(_, d)| d.windows(12).any(|w| w == b"NullsoftInst")) {
        return Err(format!("{entry} is an NSIS installer, which cannot be imported in the browser yet"));
    }
    Ok(game)
}

/// A cabinet's files (name with '/' separators, contents). Each folder (a solid
/// stream, MSZIP or LZX) is decompressed once and the files cut out of it; reading
/// them one at a time decompressed the folder from its start for each.
pub fn read_cab(c: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let bad = || "truncated or damaged cabinet".to_string();
    let u8_at = |o: usize| c.get(o).copied().ok_or_else(bad);
    let u16_at = |o: usize| c.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or_else(bad);
    let u32_at = |o: usize| c.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(bad);
    if !c.starts_with(b"MSCF") {
        return Err("not a cabinet".into());
    }
    let files_at = u32_at(16)? as usize;
    let (n_folders, n_files, flags) = (u16_at(26)? as usize, u16_at(28)? as usize, u16_at(30)?);
    if flags & 3 != 0 {
        return Err("a cabinet spanning several files".into());
    }
    let mut at = 36;
    let (mut folder_reserve, mut data_reserve) = (0, 0);
    if flags & 4 != 0 {
        let header_reserve = u16_at(36)? as usize;
        folder_reserve = u8_at(38)? as usize;
        data_reserve = u8_at(39)? as usize;
        at = 40 + header_reserve;
    }
    // folders: (first data block, blocks, compression)
    let mut folders = Vec::new();
    for _ in 0..n_folders {
        folders.push((u32_at(at)? as usize, u16_at(at + 4)? as usize, u16_at(at + 6)?));
        at += 8 + folder_reserve;
    }
    // files: (name, size, offset in its folder, folder)
    let mut files = Vec::new();
    let mut at = files_at;
    for _ in 0..n_files {
        let (size, offset, folder, attribs) = (u32_at(at)? as usize, u32_at(at + 4)? as usize, u16_at(at + 8)? as usize, u16_at(at + 14)?);
        let end = at + 16 + c.get(at + 16..).ok_or_else(bad)?.iter().position(|&b| b == 0).ok_or_else(bad)?;
        files.push((decode_name(&c[at + 16..end], attribs & 0x80 != 0).replace('\\', "/"), size, offset, folder));
        at = end + 1;
    }
    let mut out = Vec::new();
    for (fi, &(first, blocks, ctype)) in folders.iter().enumerate() {
        if !files.iter().any(|f| f.3 == fi) {
            continue;
        }
        let mut stream = Vec::new();
        let mut lzx = match ctype & 0xf {
            3 => {
                use lzxd::WindowSize as W;
                let w = match (ctype >> 8) & 0x1f {
                    15 => W::KB32,
                    16 => W::KB64,
                    17 => W::KB128,
                    18 => W::KB256,
                    19 => W::KB512,
                    20 => W::MB1,
                    21 => W::MB2,
                    22 => W::MB4,
                    23 => W::MB8,
                    24 => W::MB16,
                    25 => W::MB32,
                    b => return Err(format!("LZX window of 2^{b} bytes")),
                };
                Some(lzxd::Lzxd::new(w))
            }
            0 | 1 => None,
            t => return Err(format!("compression type {t} (Quantum) is not supported")),
        };
        let mut inflate = flate2::Decompress::new(false);
        let mut at = first;
        for _ in 0..blocks {
            let (packed, unpacked) = (u16_at(at + 4)? as usize, u16_at(at + 6)? as usize);
            let data = c.get(at + 8 + data_reserve..at + 8 + data_reserve + packed).ok_or_else(bad)?;
            at += 8 + data_reserve + packed;
            match (ctype & 0xf, lzx.as_mut()) {
                (_, Some(l)) => stream.extend_from_slice(l.decompress_next(data, unpacked).map_err(|e| format!("LZX: {e}"))?),
                (0, None) => stream.extend_from_slice(data),
                _ => {
                    // MSZIP: "CK", then a deflate stream that may refer back into the
                    // previous block (up to 32 KiB), given to it as a stored block
                    if !data.starts_with(b"CK") {
                        return Err("MSZIP block without its signature".into());
                    }
                    inflate.reset(false);
                    let dict = &stream[stream.len().saturating_sub(0x8000)..];
                    if !dict.is_empty() {
                        let mut chunk = vec![0u8];
                        chunk.extend_from_slice(&(dict.len() as u16).to_le_bytes());
                        chunk.extend_from_slice(&(!(dict.len() as u16)).to_le_bytes());
                        chunk.extend_from_slice(dict);
                        let mut sink = Vec::with_capacity(dict.len());
                        inflate.decompress_vec(&chunk, &mut sink, flate2::FlushDecompress::Sync).map_err(|e| format!("MSZIP: {e}"))?;
                    }
                    let mut block = Vec::with_capacity(unpacked);
                    inflate.decompress_vec(&data[2..], &mut block, flate2::FlushDecompress::Finish).map_err(|e| format!("MSZIP: {e}"))?;
                    if block.len() != unpacked {
                        return Err("MSZIP block of the wrong size".into());
                    }
                    stream.extend_from_slice(&block);
                }
            }
        }
        for (name, size, offset, _) in files.iter().filter(|f| f.3 == fi) {
            let data = stream.get(*offset..offset + size).ok_or_else(bad)?;
            out.push((name.clone(), data.to_vec()));
        }
    }
    Ok(out)
}

fn rcdata_string(pe: &Pe, name: &str) -> Option<String> {
    let res = pe.resources().ok()?;
    let r = res.iter().find(|r| r.kind == ResId::Id(RT_RCDATA) && r.name == ResId::Name(name.into()))?;
    let bytes = pe.resource_data(r).ok()?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// If a top-level exe is an IExpress package, replaces it with its cabinet's files and
/// returns what it would have run.
fn unwrap_iexpress(files: &mut Vec<(String, Vec<u8>)>) -> Result<Option<(String, String, String)>, String> {
    for i in 0..files.len() {
        let (path, data) = &files[i];
        if path.contains('/') || !path.to_lowercase().ends_with(".exe") {
            continue;
        }
        let Ok(pe) = Pe::parse(data) else { continue };
        let Ok(res) = pe.resources() else { continue };
        let Some(r) = res.iter().find(|r| r.kind == ResId::Id(RT_RCDATA) && r.name == ResId::Name("CABINET".into())) else { continue };
        let Ok(cab_bytes) = pe.resource_data(r) else { continue };
        if !cab_bytes.starts_with(b"MSCF") {
            continue;
        }
        let unpacked = read_cab(cab_bytes).map_err(|e| format!("the IExpress cabinet in {path}: {e}"))?;
        let info = (path.clone(), rcdata_string(&pe, "RUNPROGRAM").unwrap_or_default().trim().trim_matches('"').to_owned(), rcdata_string(&pe, "TITLE").unwrap_or_default());
        files.remove(i);
        for (p, d) in unpacked {
            files.retain(|(q, _)| !q.eq_ignore_ascii_case(&p));
            files.push((p, d));
        }
        return Ok(Some(info));
    }
    Ok(None)
}

/// Of a game shipped as a Direct3D 8 and a Direct3D 9 build (GameMaker 8.2's gm82dx9 patches
/// the runner to Direct3D 9 at run time; the runtime keeps no translations of patched code),
/// the Direct3D 8 one: the exe named like another but with "dx8" for "dx9".
pub fn dx8_build(exes: &[&str]) -> Option<String> {
    exes.iter().find(|a| {
        let l = a.to_lowercase();
        l.contains("dx8") && exes.iter().any(|b| b.to_lowercase() == l.replace("dx8", "dx9"))
    }).map(|a| a.to_string())
}

/// A path in a folder named as an old version ("(Old version)", "old"): another copy is
/// the one to play.
fn old_copy(path: &str) -> bool {
    path.to_lowercase().split(|ch: char| !ch.is_alphanumeric()).any(|w| w == "old" || w == "older" || w == "previous")
}

/// An exe that may be the game (not an uninstaller or a runtime's installer).
fn game_exe(path: &str) -> bool {
    let l = path.to_lowercase();
    let file = l.rsplit('/').next().unwrap_or(&l);
    l.ends_with(".exe") && !["unins", "vcredist", "vc_redist", "dxsetup", "dxwebsetup", "oalinst", "dotnet", "directx"].iter().any(|p| file.starts_with(p))
}

impl Game {
    /// A game whose exe is not at the top (a folder in the shared folder, a readme beside
    /// the game's folder): the folder of the exe to run becomes the top, and what lies
    /// outside it is left out. The exe is one GameMaker made if any, not in an old
    /// version's folder, the shallowest, then the largest.
    fn reroot(&mut self) {
        if self.files.iter().any(|(p, _)| !p.contains('/') && game_exe(p)) {
            return;
        }
        let best = self
            .files
            .iter()
            .filter(|(p, _)| game_exe(p))
            .max_by_key(|(p, d)| (self.engine(p)["kind"] != "unknown", !old_copy(p), std::cmp::Reverse(p.matches('/').count()), d.len()))
            .map(|(p, _)| p.clone());
        let Some((dir, _)) = best.as_deref().and_then(|b| b.rsplit_once('/')) else { return };
        let prefix = format!("{dir}/");
        let files = std::mem::take(&mut self.files);
        self.files = files.into_iter().filter_map(|(p, d)| p.strip_prefix(&prefix).map(|q| (q.to_owned(), d))).collect();
        if let Some(name) = dir.rsplit('/').next() {
            self.title = name.to_owned();
        }
    }

    fn find(&self, name: &str) -> Option<&(String, Vec<u8>)> {
        self.files.iter().find(|(p, _)| p.eq_ignore_ascii_case(name))
    }

    /// The exe to run (the IExpress program, else the largest top-level exe) and the
    /// other candidates.
    pub fn entry(&self) -> (String, Vec<String>) {
        let top: Vec<&(String, Vec<u8>)> = self
            .files
            .iter()
            .filter(|(p, _)| !p.contains('/') && game_exe(p))
            .collect();
        let candidates = top.iter().map(|(p, _)| p.clone()).collect();
        if let Some((_, run, _)) = &self.iexpress {
            if let Some((p, _)) = self.find(run) {
                return (p.clone(), candidates);
            }
        }
        if let Some(p) = dx8_build(&top.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>()) {
            return (p, candidates);
        }
        let largest = top.iter().max_by_key(|(_, d)| d.len()).map(|(p, _)| p.clone()).unwrap_or_default();
        (largest, candidates)
    }

    /// The runner, as the native importer's `engine::detect` reports it.
    pub fn engine(&self, entry: &str) -> Value {
        let Some((_, exe)) = self.find(entry) else { return json!({ "kind": "unknown" }) };
        // (GM8.0: the data header where the runner looks, at 1,980,000 or every 10,000
        // bytes from 2,000,000 on: runner builds differ in size)
        let gm80 = std::iter::once(1_980_000).chain((2_000_000..exe.len()).step_by(10_000)).any(|p| u32_at(exe, p) == Some(1_234_321) && u32_at(exe, p + 4) == Some(800));
        if gm80 {
            return json!({ "kind": "gm80" });
        }
        const GM81: (usize, [u8; 8]) = (0x226CF3, [0xE8, 0x80, 0xF2, 0xDD, 0xFF, 0xC7, 0x45, 0xF0]);
        if exe.get(GM81.0..GM81.0 + 8) == Some(&GM81.1[..]) {
            return json!({ "kind": "gm81" });
        }
        let dir = entry.rsplit_once('/').map_or(String::new(), |(d, _)| format!("{d}/"));
        for name in ["data.win", "game.win"] {
            if let Some(e) = self.find(&format!("{dir}{name}")).and_then(|(_, d)| gms_form(d, name, pe64(exe))) {
                return e;
            }
        }
        // YYC builds can embed the FORM in the exe
        let mut from = 0;
        while let Some(pos) = exe[from..].windows(4).position(|w| w == b"FORM").map(|p| p + from) {
            if exe.get(pos + 8..pos + 12) == Some(b"GEN8") {
                if let Some(e) = gms_form(&exe[pos..], &format!("embedded@{pos}"), pe64(exe)) {
                    return e;
                }
            }
            from = pos + 4;
        }
        json!({ "kind": "unknown" })
    }

    /// System DLLs the game's modules need (static imports; redistributable runtimes
    /// also by name in their strings) that it neither ships nor already has under
    /// C/Windows/system32 and the runtime does not emulate.
    pub fn needs(&self, shaders: bool) -> Vec<String> {
        use runtime::hle::kernel32::{CORE_DLLS, HLE_DLLS};
        let have: Vec<String> = self
            .files
            .iter()
            .filter_map(|(p, _)| if !p.contains('/') { Some(p.to_lowercase()) } else { p.to_lowercase().strip_prefix("c/windows/system32/").map(str::to_owned) })
            .collect();
        let redistributable = |n: &str| ["msvcr", "msvcp", "vcruntime", "concrt", "ucrtbase"].iter().any(|p| n.starts_with(p)) || (shaders && n.starts_with("d3dcompiler_"));
        let mut out: Vec<String> = Vec::new();
        for (p, d) in &self.files {
            let l = p.to_lowercase();
            if !(l.ends_with(".exe") || l.ends_with(".dll")) {
                continue;
            }
            // (the system DLLs are 32-bit: a 64-bit module's imports go to the HLE)
            let Ok(pe) = Pe::parse(d) else { continue };
            if pe.pe64 {
                continue;
            }
            let mut wanted: Vec<String> = pe.imports().unwrap_or_default().iter().map(|i| i.dll.to_lowercase()).collect();
            wanted.extend(dll_names_in(d).into_iter().filter(|n| redistributable(n)));
            for name in wanted {
                let name = runtime::api_set(&name).map_or(name, str::to_owned);
                let stem = name.trim_end_matches(".dll");
                if CORE_DLLS.contains(&stem) || HLE_DLLS.contains(&stem) || have.contains(&name) || out.contains(&name) {
                    continue;
                }
                out.push(name);
            }
        }
        out
    }

    pub fn manifest(&self) -> Value {
        let (entry, candidates) = self.entry();
        let engine = self.engine(&entry);
        // (a title of no ASCII letters: the archive's name)
        let mut slug = slugify(&self.title);
        if slug.is_empty() {
            let stem = self.source.rsplit(['/', '\\']).next().unwrap_or_default();
            slug = slugify(stem.rsplit_once('.').map_or(stem, |(s, _)| s));
        }
        json!({
            "slug": slug,
            "title": self.title,
            "source": self.source,
            "engine": engine,
            "entry": entry,
            "entry_candidates": candidates,
            "iexpress": self.iexpress.as_ref().map(|(stub, run, title)| json!({ "stub": stub, "run_program": run, "title": title })),
            "files": self.files.iter().map(|(p, d)| json!({ "path": p, "size": d.len() })).collect::<Vec<_>>(),
        })
    }
}

fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    d.get(off..off + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// A PE32+ (64-bit) image: optional header magic 0x20b.
fn pe64(exe: &[u8]) -> bool {
    let Some(pe) = u32_at(exe, 0x3c) else { return false };
    exe.get(pe as usize..pe as usize + 4) == Some(b"PE\0\0") && exe.get(pe as usize + 24..pe as usize + 26) == Some(&[0x0b, 0x02][..])
}

fn gms_form(d: &[u8], name: &str, x64: bool) -> Option<Value> {
    if d.get(0..4) != Some(b"FORM") {
        return None;
    }
    let end = (8 + u32_at(d, 4)? as usize).min(d.len());
    let mut off = 8;
    let (mut gen8, mut code_size, mut shaders) = (None, None, 0);
    while off + 8 <= end {
        let size = u32_at(d, off + 4)? as usize;
        match &d[off..off + 4] {
            b"GEN8" => gen8 = Some(off + 8),
            b"CODE" => code_size = Some(size),
            b"SHDR" if size >= 4 => shaders = u32_at(d, off + 8)?,
            _ => {}
        }
        off += 8 + size;
    }
    let g = gen8?;
    Some(json!({
        "kind": "gms",
        "bytecode": *d.get(g + 1)?,
        "version": [u32_at(d, g + 44)?, u32_at(d, g + 48)?, u32_at(d, g + 52)?, u32_at(d, g + 56)?],
        "yyc": code_size.unwrap_or(0) == 0,
        "data_file": name,
        "shaders": shaders,
        "x64": x64,
    }))
}

/// "*.dll" names in a binary's ASCII and UTF-16 strings (lowercase).
fn dll_names_in(bytes: &[u8]) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    let ok = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.';
    for step in [1usize, 2] {
        let mut i = 0;
        while i + 4 * step <= bytes.len() {
            let at = |k: usize| bytes[i + k * step];
            let is_dll = (0..4).all(|k| step == 1 || bytes.get(i + k * step + 1) == Some(&0))
                && at(0) == b'.'
                && at(1).eq_ignore_ascii_case(&b'd')
                && at(2).eq_ignore_ascii_case(&b'l')
                && at(3).eq_ignore_ascii_case(&b'l');
            if is_dll {
                let mut start = i;
                while start >= step && ok(bytes[start - step]) && (step == 1 || bytes[start - step + 1] == 0) {
                    start -= step;
                }
                let name: String = (start..i + 4 * step).step_by(step).map(|k| bytes[k] as char).collect();
                if name.len() > 4 && name.len() < 64 {
                    out.insert(name.to_lowercase());
                }
            }
            i += step;
        }
    }
    out.into_iter().collect()
}

pub fn slugify(title: &str) -> String {
    let mut s = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    s.trim_end_matches('-').to_owned()
}

// ---- exports for the page (one import at a time) ---------------------------------------------

struct State {
    game: Option<Game>,
    out: Vec<u8>,
}

static mut STATE: State = State { game: None, out: Vec::new() };

#[allow(static_mut_refs)]
fn state() -> &'static mut State {
    // Single-threaded: the page's one instance.
    unsafe { &mut STATE }
}

fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(ptr, len) } }
}

fn set_out(s: String) {
    state().out = s.into_bytes();
}

/// `len` bytes for the page to fill (freed with `dealloc`).
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut v = vec![0u8; len.max(1)];
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[unsafe(no_mangle)]
pub extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    unsafe { drop(Vec::from_raw_parts(ptr, len.max(1), len.max(1))) };
}

/// Unpacks the zip at `ptr` (named `name`); 0, or 1 with the error as the output.
#[unsafe(no_mangle)]
pub extern "C" fn import_zip(ptr: *const u8, len: usize, name_ptr: *const u8, name_len: usize) -> u32 {
    let name = String::from_utf8_lossy(bytes(name_ptr, name_len)).into_owned();
    match import(bytes(ptr, len), &name) {
        Ok(g) => {
            state().game = Some(g);
            0
        }
        Err(e) => {
            set_out(e);
            1
        }
    }
}

/// The system DLLs to add (a JSON array) as the output.
#[unsafe(no_mangle)]
pub extern "C" fn needs() {
    let Some(g) = state().game.as_ref() else { return set_out("[]".into()) };
    let (entry, _) = g.entry();
    let shaders = g.engine(&entry)["shaders"].as_u64().unwrap_or(0) > 0;
    set_out(Value::from(g.needs(shaders)).to_string());
}

/// Adds a file (a system DLL under C/Windows/system32).
#[unsafe(no_mangle)]
pub extern "C" fn add_file(path_ptr: *const u8, path_len: usize, ptr: *const u8, len: usize) {
    let path = String::from_utf8_lossy(bytes(path_ptr, path_len)).into_owned();
    if let Some(g) = state().game.as_mut() {
        g.files.retain(|(p, _)| !p.eq_ignore_ascii_case(&path));
        g.files.push((path, bytes(ptr, len).to_vec()));
    }
}

/// The manifest (JSON, as the native importer writes it) as the output.
#[unsafe(no_mangle)]
pub extern "C" fn manifest() {
    let m = state().game.as_ref().map_or(Value::Null, Game::manifest);
    set_out(m.to_string());
}

#[unsafe(no_mangle)]
pub extern "C" fn file_count() -> u32 {
    state().game.as_ref().map_or(0, |g| g.files.len() as u32)
}

/// File `i`'s path as the output.
#[unsafe(no_mangle)]
pub extern "C" fn file_path(i: u32) {
    let p = state().game.as_ref().and_then(|g| g.files.get(i as usize)).map(|(p, _)| p.clone()).unwrap_or_default();
    set_out(p);
}

#[unsafe(no_mangle)]
pub extern "C" fn file_ptr(i: u32) -> *const u8 {
    state().game.as_ref().and_then(|g| g.files.get(i as usize)).map_or(std::ptr::null(), |(_, d)| d.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn file_len(i: u32) -> u32 {
    state().game.as_ref().and_then(|g| g.files.get(i as usize)).map_or(0, |(_, d)| d.len() as u32)
}

/// Drops the game (after the page copied it out).
#[unsafe(no_mangle)]
pub extern "C" fn finish() {
    state().game = None;
}

#[unsafe(no_mangle)]
pub extern "C" fn out_ptr() -> *const u8 {
    state().out.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn out_len() -> u32 {
    state().out.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let raw = [0x83, 0x65, 0x83, 0x58, 0x83, 0x67, b'.', b't', b'x', b't'];
        assert_eq!(decode_name(&raw, false), "テスト.txt");
        assert!(safe_parts("../evil").is_none());
        assert_eq!(safe_parts("a\\b.txt"), Some(vec!["a".to_string(), "b.txt".to_string()]));
        assert_eq!(slugify("I wanna run the Marathon v1.2"), "i-wanna-run-the-marathon-v1-2");
    }
}
