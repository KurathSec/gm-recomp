//! Maps PE images into guest memory, applies relocations and binds imports to
//! HLE stubs or to exports of other loaded images.

use crate::mem::{Guest, PAGE_EXECUTE_READWRITE, Vm};
use pe::{ImportName, Pe};
use crate::fasthash::HashMap;

#[derive(Debug, Clone)]
pub struct LoadedModule {
    pub name: String,
    pub path: String,
    pub base: u32,
    pub size: u32,
    pub entry: u32,
    pub is_dll: bool,
    /// Export name -> VA. Ordinal-only exports use "#<ordinal>".
    pub exports: HashMap<String, u32>,
    /// VA ranges that hold code (for discovery/translation).
    pub code_ranges: Vec<(u32, u32)>,
    /// File bytes (kept for discovery).
    pub file: Vec<u8>,
    pub tls_callbacks: Vec<u32>,
    pub tls_index_va: u32,
    /// Static TLS template (start VA, end VA, zero fill), relocated.
    pub tls_template: Option<(u32, u32, u32)>,
    /// (DLL, function, IAT slot VA) for every import.
    pub imports: Vec<(String, String, u32)>,
    /// A 64-bit (PE32+) image: 8-byte pointers, x86-64 code (docs/x64.md).
    pub pe64: bool,
    /// The upper half of its addresses: a 64-bit image stays at its preferred base above
    /// 4 GB, `base` being where that falls in the 32-bit guest memory (docs/x64.md).
    pub base_high: u32,
}

pub struct LoadRequest<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub bytes: Vec<u8>,
}

/// Resolves an import to a guest address: an HLE stub or another module's export.
pub trait ImportResolver {
    fn resolve(&mut self, dll: &str, name: &str) -> u32;
}

pub fn load(req: LoadRequest, vm: &mut Vm, mem: &mut dyn Guest, resolver: &mut dyn ImportResolver) -> Result<LoadedModule, String> {
    let bytes = req.bytes;
    let pe = Pe::parse(&bytes).map_err(|e| format!("{}: {e}", req.name))?;
    let size = pe.size_of_image.div_ceil(0x1000) * 0x1000;
    // A 64-bit image stays at its preferred base above 4 GB, which memory addresses by its
    // low 32 bits (docs/x64.md): it is mapped there, unrelocated.
    let preferred = if pe.pe64 { pe.image_base64 as u32 } else { pe.image_base };
    let mut base = vm.alloc(preferred, size, true, PAGE_EXECUTE_READWRITE, false, "image").unwrap_or(0);
    let mut base_high = if pe.pe64 && base != 0 { (pe.image_base64 >> 32) as u32 } else { 0 };
    if base == 0 {
        if pe.relocs_stripped() {
            return Err(format!("{}: preferred base {:#x} is occupied and the image has no relocations", req.name, pe.image_base));
        }
        base = vm.alloc(0, size, true, PAGE_EXECUTE_READWRITE, false, "image").ok_or("out of address space")?;
    }
    mem.ensure_mapped(base, size).map_err(|e| e.to_string())?;
    mem.fill(base, size, 0).map_err(|e| e.to_string())?;
    let hdr = (pe.size_of_headers as usize).min(bytes.len());
    mem.write(base, &bytes[..hdr]).map_err(|e| e.to_string())?;
    let mut code_ranges = Vec::new();
    for s in &pe.sections {
        let raw = bytes.get(s.raw_offset as usize..(s.raw_offset + s.raw_size.min(s.mapped_size())) as usize).unwrap_or(&[]);
        if !raw.is_empty() {
            mem.write(base + s.virtual_address, raw).map_err(|e| e.to_string())?;
        }
        if s.is_executable() {
            code_ranges.push((base + s.virtual_address, base + s.virtual_address + s.mapped_size()));
        }
    }
    // Relocations (64-bit addresses in a PE32+ image, moved below 4 GB when its own place
    // there is taken).
    let delta = base.wrapping_sub(pe.image_base);
    if pe.pe64 && base != preferred {
        base_high = 0;
        let delta64 = (base as u64).wrapping_sub(pe.image_base64);
        for rva in pe.relocations().map_err(|e| e.to_string())? {
            let va = base + rva;
            let v = mem.read_u32(va).map_err(|e| e.to_string())? as u64 | (mem.read_u32(va + 4).map_err(|e| e.to_string())? as u64) << 32;
            let v = v.wrapping_add(delta64);
            mem.write_u32(va, v as u32).map_err(|e| e.to_string())?;
            mem.write_u32(va + 4, (v >> 32) as u32).map_err(|e| e.to_string())?;
        }
    } else if !pe.pe64 && delta != 0 {
        for rva in pe.relocations().map_err(|e| e.to_string())? {
            let va = base + rva;
            let v = mem.read_u32(va).map_err(|e| e.to_string())?;
            mem.write_u32(va, v.wrapping_add(delta)).map_err(|e| e.to_string())?;
        }
    }
    // Imports.
    let mut imports = Vec::new();
    for dll in pe.imports().map_err(|e| e.to_string())? {
        let dll_name = dll.dll.to_lowercase();
        for imp in &dll.entries {
            let fname = match &imp.name {
                ImportName::Name { name, .. } => name.clone(),
                ImportName::Ordinal(o) => format!("#{o}"),
            };
            let target = resolver.resolve(&dll_name, &fname);
            let slot = base + imp.iat_rva;
            mem.write_u32(slot, target).map_err(|e| e.to_string())?;
            if pe.pe64 {
                mem.write_u32(slot + 4, 0).map_err(|e| e.to_string())?;
            }
            imports.push((dll_name.clone(), fname, slot));
        }
    }
    let mut exports = HashMap::default();
    for e in pe.exports().map_err(|e| e.to_string())? {
        if e.forwarder.is_some() {
            continue;
        }
        let va = base + e.rva;
        if let Some(n) = &e.name {
            exports.insert(n.clone(), va);
        }
        exports.insert(format!("#{}", e.ordinal), va);
    }
    let tls = pe.tls().map_err(|e| e.to_string())?;
    let pe64 = pe.pe64;
    let (tls_callbacks, tls_index_va, tls_template) = match tls {
        Some(t) => (
            t.callbacks.iter().map(|c| c.wrapping_add(delta)).collect(),
            t.index_va.wrapping_add(delta),
            Some((t.start_va.wrapping_add(delta), t.end_va.wrapping_add(delta), t.zero_fill)),
        ),
        None => (Vec::new(), 0, None),
    };
    Ok(LoadedModule {
        name: req.name.to_lowercase(),
        path: req.path.to_string(),
        base,
        size,
        entry: base + pe.entry_rva,
        is_dll: pe.is_dll(),
        exports,
        code_ranges,
        file: bytes,
        tls_callbacks,
        tls_index_va,
        tls_template,
        imports,
        pe64,
        base_high,
    })
}
