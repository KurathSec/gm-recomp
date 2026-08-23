//! Per-module facts the recompiler and HLE work is planned from: sections,
//! static and delay imports, exports, TLS, packer hints, and the API names a
//! module is likely to resolve at runtime through LoadLibrary/GetProcAddress.

use anyhow::Result;
use pe::Pe;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct SectionInfo {
    pub name: String,
    pub rva: u32,
    pub virtual_size: u32,
    pub raw_size: u32,
    pub exec: bool,
    pub write: bool,
    pub entropy: f32,
}

#[derive(Debug, Serialize)]
pub struct ImportInfo {
    pub dll: String,
    pub delay_load: bool,
    pub functions: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Module {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    /// Hash over executable sections only: identical runners share recompiled code.
    pub code_sha256: String,
    pub image_base: u32,
    pub entry_rva: u32,
    pub is_dll: bool,
    pub relocs_stripped: bool,
    pub aslr: bool,
    pub subsystem: u16,
    pub timestamp: u32,
    pub sections: Vec<SectionInfo>,
    pub imports: Vec<ImportInfo>,
    pub exports: Vec<String>,
    pub tls_callbacks: Vec<u32>,
    pub overlay_size: usize,
    pub clr: bool,
    /// DLL names that appear as strings (ASCII or UTF-16) but are not statically imported.
    pub dynamic_dlls: Vec<String>,
    /// Exports of known system DLLs whose names appear as strings: likely GetProcAddress targets.
    pub dynamic_api_candidates: BTreeMap<String, Vec<String>>,
    pub packer_hints: Vec<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn entropy(data: &[u8]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let n = data.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| -(c as f64 / n) * (c as f64 / n).log2()).sum::<f64>() as f32
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// Finds "*.dll" strings in both ASCII and UTF-16LE.
pub fn dll_strings(data: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let lower: Vec<u8> = data.iter().map(|b| b.to_ascii_lowercase()).collect();
    for (i, w) in lower.windows(4).enumerate() {
        if w != b".dll" {
            continue;
        }
        let mut s = i;
        while s > 0 && is_name_byte(data[s - 1]) && i - s < 64 {
            s -= 1;
        }
        let name = &lower[s..i + 4];
        if name.len() > 4 && name[0] != b'.' {
            out.insert(String::from_utf8_lossy(name).into_owned());
        }
    }
    for (i, w) in lower.windows(8).enumerate() {
        if w != b".\0d\0l\0l\0" {
            continue;
        }
        let mut s = i;
        while s >= 2 && data[s - 1] == 0 && is_name_byte(data[s - 2]) && i - s < 128 {
            s -= 2;
        }
        if s < i {
            let name: String = lower[s..i + 8].iter().step_by(2).map(|&b| b as char).collect();
            if !name.starts_with('.') {
                out.insert(name);
            }
        }
    }
    out
}

/// NUL-terminated ASCII identifiers of length 4..64, the shape of API names.
fn identifier_strings(data: &[u8]) -> BTreeSet<&[u8]> {
    let mut out = BTreeSet::new();
    let mut start = 0;
    for (i, &b) in data.iter().enumerate() {
        if b.is_ascii_alphanumeric() || b == b'_' {
            continue;
        }
        let len = i - start;
        if b == 0 && (4..64).contains(&len) && data[start].is_ascii_alphabetic() {
            out.insert(&data[start..i]);
        }
        start = i + 1;
    }
    out
}

/// Export names of the system DLLs, read from Wine's PE builds.
pub struct SystemExports {
    by_dll: HashMap<String, BTreeSet<String>>,
}

impl SystemExports {
    pub fn load(dir: &Path) -> Self {
        let mut by_dll = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if !name.ends_with(".dll") {
                    continue;
                }
                let Ok(bytes) = std::fs::read(entry.path()) else { continue };
                let Ok(pe) = Pe::parse(&bytes) else { continue };
                let names: BTreeSet<String> =
                    pe.exports().unwrap_or_default().into_iter().filter_map(|e| e.name).collect();
                by_dll.insert(name, names);
            }
        }
        SystemExports { by_dll }
    }

    pub fn is_empty(&self) -> bool {
        self.by_dll.is_empty()
    }

    pub fn exports_of(&self, dll: &str) -> Option<&BTreeSet<String>> {
        self.by_dll.get(&dll.to_lowercase())
    }
}

pub fn analyze(path_rel: &str, bytes: &[u8], sys: &SystemExports) -> Result<Module> {
    let pe = Pe::parse(bytes)?;
    let mut code_hash = Sha256::new();
    let mut sections = Vec::new();
    for s in &pe.sections {
        let raw = bytes.get(s.raw_offset as usize..(s.raw_offset + s.raw_size) as usize).unwrap_or(&[]);
        if s.is_executable() {
            code_hash.update(raw);
        }
        sections.push(SectionInfo {
            name: s.name.clone(),
            rva: s.virtual_address,
            virtual_size: s.virtual_size,
            raw_size: s.raw_size,
            exec: s.is_executable(),
            write: s.is_writable(),
            entropy: entropy(raw),
        });
    }
    let imports: Vec<ImportInfo> = pe
        .imports()?
        .into_iter()
        .map(|d| ImportInfo {
            dll: d.dll.to_lowercase(),
            delay_load: d.delay_load,
            functions: d.entries.iter().map(|e| e.name.to_string()).collect(),
        })
        .collect();
    let static_dlls: BTreeSet<String> = imports.iter().map(|i| i.dll.clone()).collect();
    let own_name = Path::new(path_rel).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let dynamic_dlls: Vec<String> =
        dll_strings(bytes).into_iter().filter(|d| !static_dlls.contains(d) && *d != own_name).collect();

    let own_exports: BTreeSet<String> = pe.exports()?.into_iter().filter_map(|e| e.name).collect();
    let mut idents = identifier_strings(bytes);
    // A DLL's own export-name table would otherwise match a sibling DLL with the same API.
    idents.retain(|i| !own_exports.contains(std::str::from_utf8(i).unwrap_or("")));
    let mut dynamic_api_candidates = BTreeMap::new();
    for dll in &dynamic_dlls {
        if let Some(exports) = sys.exports_of(dll) {
            let hits: Vec<String> = exports.iter().filter(|e| idents.contains(e.as_bytes())).cloned().collect();
            if !hits.is_empty() {
                dynamic_api_candidates.insert(dll.clone(), hits);
            }
        }
    }

    let mut packer_hints = Vec::new();
    for s in &sections {
        let n = s.name.to_lowercase();
        if ["upx", "aspack", ".enigma", ".vmp", ".themida", "pec", ".nsp", "mpress"].iter().any(|p| n.starts_with(p)) {
            packer_hints.push(format!("section name {}", s.name));
        }
        if s.exec && s.raw_size == 0 && s.virtual_size > 0 {
            packer_hints.push(format!("executable section {} has no file data (unpacked at runtime)", s.name));
        }
        if s.exec && s.raw_size > 4096 && s.entropy > 7.2 {
            packer_hints.push(format!("executable section {} has entropy {:.2}", s.name, s.entropy));
        }
    }
    // Packers typically start in a section that is both writable and executable
    // (UPX1, .aspack...); Delphi's read-only .itext entry is normal.
    match pe.section_of(pe.entry_rva) {
        Some(s) if !s.is_executable() => packer_hints.push(format!("entry point in non-executable section {}", s.name)),
        Some(s) if s.is_writable() => packer_hints.push(format!("entry point in writable code section {}", s.name)),
        None if pe.entry_rva != 0 => packer_hints.push("entry point outside all sections".into()),
        _ => {}
    }

    Ok(Module {
        path: path_rel.to_owned(),
        size: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
        code_sha256: hex(&code_hash.finalize()),
        image_base: pe.image_base,
        entry_rva: pe.entry_rva,
        is_dll: pe.is_dll(),
        relocs_stripped: pe.relocs_stripped(),
        aslr: pe.dll_characteristics & 0x40 != 0,
        subsystem: pe.subsystem,
        timestamp: pe.timestamp,
        sections,
        imports,
        exports: pe.exports()?.into_iter().map(|e| e.name.unwrap_or_else(|| format!("#{}", e.ordinal))).collect(),
        tls_callbacks: pe.tls()?.map(|t| t.callbacks).unwrap_or_default(),
        overlay_size: pe.overlay().len(),
        clr: pe.dir(pe::DIR_CLR).rva != 0,
        dynamic_dlls,
        dynamic_api_candidates,
        packer_hints,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ascii_and_utf16_dll_names() {
        let mut d = b"xx\0d3d9.dll\0yy".to_vec();
        for c in "XInput1_4.DLL".encode_utf16() {
            d.extend_from_slice(&c.to_le_bytes());
        }
        let got = dll_strings(&d);
        assert!(got.contains("d3d9.dll"));
        assert!(got.contains("xinput1_4.dll"));
    }

    #[test]
    fn identifiers_need_nul_terminator() {
        let ids = identifier_strings(b"\0Direct3DCreate9\0abc\0SomethingElse");
        assert!(ids.contains(&b"Direct3DCreate9"[..]));
        assert!(!ids.contains(&b"abc"[..]));
        assert!(!ids.contains(&b"SomethingElse"[..]));
    }
}
