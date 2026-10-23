//! The long tail: advapi32 (in-memory registry), ole32/oleaut32 (BSTR, variants),
//! shell32, version, winmm (timers; audio arrives in R4), imm32, comctl32,
//! comdlg32 and the DirectX/network entry points that report "not available".

use super::{ApiResult, Ctx, Registry, Ret, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use crate::const_apis;
use crate::mem::Guest;
use crate::process::Handle;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

pub const S_OK: u32 = 0;
pub const S_FALSE: u32 = 1;
pub const E_NOTIMPL: u32 = 0x8000_4001;
pub const E_NOINTERFACE: u32 = 0x8000_4002;
pub const E_FAIL: u32 = 0x8000_4005;
pub const E_INVALIDARG: u32 = 0x8007_0057;
pub const E_OUTOFMEMORY: u32 = 0x8007_000E;
pub const REGDB_E_CLASSNOTREG: u32 = 0x8004_0154;
pub const ERROR_MORE_DATA: u32 = 234;
pub const ERROR_NO_MORE_ITEMS: u32 = 259;

fn galloc(c: &mut Ctx, size: u32) -> u32 {
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    c.p.heap_alloc(g, heap, size, true)
}

// ---- registry -------------------------------------------------------------------

fn root_name(h: u32) -> Option<&'static str> {
    Some(match h {
        0x8000_0000 => "HKEY_CLASSES_ROOT",
        0x8000_0001 => "HKEY_CURRENT_USER",
        0x8000_0002 => "HKEY_LOCAL_MACHINE",
        0x8000_0003 => "HKEY_USERS",
        0x8000_0005 => "HKEY_CURRENT_CONFIG",
        _ => return None,
    })
}

fn key_path(c: &Ctx, h: u32) -> Option<String> {
    if let Some(r) = root_name(h) {
        return Some(r.to_string());
    }
    match c.p.handles.get(&h) {
        Some(Handle::Registry(p)) => Some(p.clone()),
        _ => None,
    }
}

fn open_key(c: &mut Ctx, parent: u32, sub: &str, out: u32) -> u32 {
    let Some(base) = key_path(c, parent) else { return super::ERROR_INVALID_HANDLE };
    let sub = sub.trim_matches('\\');
    let path = if sub.is_empty() { base } else { format!("{base}\\{sub}") };
    let h = c.p.new_handle(Handle::Registry(path));
    let _ = c.host.write_u32(out, h);
    ERROR_SUCCESS
}

fn query_value(c: &mut Ctx, key: u32, name: &str, ptype: u32, data: u32, pcb: u32) -> u32 {
    let Some(path) = key_path(c, key) else { return super::ERROR_INVALID_HANDLE };
    let full = format!("{path}\\{name}").to_lowercase();
    c.trace(format!("({full:?})"));
    let Some(v) = c.p.registry_values.get(&full).cloned() else { return ERROR_FILE_NOT_FOUND };
    let ty = u32::from_le_bytes(v[..4].try_into().unwrap());
    let bytes = &v[4..];
    if ptype != 0 {
        let _ = c.host.write_u32(ptype, ty);
    }
    let cap = if pcb != 0 { c.host.read_u32(pcb).unwrap_or(0) } else { 0 };
    if pcb != 0 {
        let _ = c.host.write_u32(pcb, bytes.len() as u32);
    }
    if data == 0 {
        return ERROR_SUCCESS;
    }
    if (cap as usize) < bytes.len() {
        return ERROR_MORE_DATA;
    }
    let _ = c.host.write(data, bytes);
    ERROR_SUCCESS
}

/// RegGetValue: a value of `key`'s subkey `sub`, without opening it first.
fn get_value(c: &mut Ctx, key: u32, sub: &str, name: &str, ptype: u32, data: u32, pcb: u32) -> u32 {
    let Some(base) = key_path(c, key) else { return super::ERROR_INVALID_HANDLE };
    let sub = sub.trim_matches('\\');
    let path = if sub.is_empty() { base } else { format!("{base}\\{sub}") };
    let h = c.p.new_handle(Handle::Registry(path));
    let e = query_value(c, h, name, ptype, data, pcb);
    c.p.handles.remove(&h);
    e
}

/// RegSetKeyValue: a value of `key`'s subkey `sub`, without opening it first.
fn set_key_value(c: &mut Ctx, key: u32, sub: &str, name: &str, ty: u32, data: u32, cb: u32) -> u32 {
    let Some(base) = key_path(c, key) else { return super::ERROR_INVALID_HANDLE };
    let sub = sub.trim_matches('\\');
    let path = if sub.is_empty() { base } else { format!("{base}\\{sub}") };
    let h = c.p.new_handle(Handle::Registry(path));
    let e = set_value(c, h, name, ty, data, cb);
    c.p.handles.remove(&h);
    e
}

fn set_value(c: &mut Ctx, key: u32, name: &str, ty: u32, data: u32, cb: u32) -> u32 {
    let Some(path) = key_path(c, key) else { return super::ERROR_INVALID_HANDLE };
    let full = format!("{path}\\{name}").to_lowercase();
    let mut v = ty.to_le_bytes().to_vec();
    let mut buf = vec![0u8; cb as usize];
    let _ = c.host.read(data, &mut buf);
    v.extend_from_slice(&buf);
    c.trace(format!("({full:?}, type {ty}, {cb} bytes)"));
    c.p.registry_values.insert(full, v);
    ERROR_SUCCESS
}

// ---- GUIDs and BSTRs --------------------------------------------------------------

fn parse_guid(s: &str) -> Option<[u8; 16]> {
    let s = s.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let mut g = [0u8; 16];
    let a = u32::from_str_radix(parts[0], 16).ok()?;
    let b = u16::from_str_radix(parts[1], 16).ok()?;
    let cc = u16::from_str_radix(parts[2], 16).ok()?;
    g[..4].copy_from_slice(&a.to_le_bytes());
    g[4..6].copy_from_slice(&b.to_le_bytes());
    g[6..8].copy_from_slice(&cc.to_le_bytes());
    let d = u16::from_str_radix(parts[3], 16).ok()?;
    g[8..10].copy_from_slice(&d.to_be_bytes());
    let e = u64::from_str_radix(parts[4], 16).ok()?;
    g[10..16].copy_from_slice(&e.to_be_bytes()[2..]);
    Some(g)
}

fn format_guid(g: &[u8; 16]) -> String {
    let a = u32::from_le_bytes(g[..4].try_into().unwrap());
    let b = u16::from_le_bytes(g[4..6].try_into().unwrap());
    let c = u16::from_le_bytes(g[6..8].try_into().unwrap());
    format!(
        "{{{a:08X}-{b:04X}-{c:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    )
}

fn bstr_alloc(c: &mut Ctx, units: &[u16]) -> u32 {
    let bytes = units.len() as u32 * 2;
    let p = galloc(c, bytes + 6);
    if p == 0 {
        return 0;
    }
    let _ = c.host.write_u32(p, bytes);
    for (i, u) in units.iter().enumerate() {
        let _ = c.host.write_u16(p + 4 + 2 * i as u32, *u);
    }
    let _ = c.host.write_u16(p + 4 + bytes, 0);
    p + 4
}

fn bstr_free(c: &mut Ctx, b: u32) {
    if b != 0 {
        c.p.heap_free(b - 4);
    }
}

fn read_units(c: &Ctx, p: u32, len: u32) -> Vec<u16> {
    (0..len).map(|i| c.host.read_u16(p + 2 * i).unwrap_or(0)).collect()
}

fn wstr_units(c: &Ctx, p: u32) -> Vec<u16> {
    let mut v = Vec::new();
    let mut a = p;
    loop {
        let u = c.host.read_u16(a).unwrap_or(0);
        if u == 0 || v.len() > 1 << 20 {
            break;
        }
        v.push(u);
        a += 2;
    }
    v
}

fn csidl_path(csidl: u32) -> Option<&'static str> {
    Some(match csidl & 0xFF {
        0x00 => "C:\\Users\\player\\Desktop",
        0x05 => "C:\\Users\\player\\Documents",
        0x07 => "C:\\Users\\player\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Startup",
        0x0D => "C:\\Users\\player\\Music",
        0x0E => "C:\\Users\\player\\Videos",
        0x10 => "C:\\Users\\player\\Desktop",
        0x1A => "C:\\Users\\player\\AppData\\Roaming",
        0x1C => "C:\\Users\\player\\AppData\\Local",
        0x23 => "C:\\ProgramData",
        0x24 => "C:\\Windows",
        0x25 => "C:\\Windows\\system32",
        0x26 => "C:\\Program Files",
        0x27 => "C:\\Users\\player\\Pictures",
        0x28 => "C:\\Users\\player",
        0x2B => "C:\\Program Files\\Common Files",
        _ => return None,
    })
}

/// RT_VERSION resource bytes of a PE file on the guest file system.
fn version_resource(c: &mut Ctx, path: &str) -> Option<Vec<u8>> {
    let base = path.rsplit(['\\', '/']).next().unwrap_or(path).to_lowercase();
    let bytes = match c.p.module_by_name(&base) {
        Some(m) => m.file.clone(),
        None => match c.p.fs.read_all(&c.p.vpath(path)) {
            Some(b) => b,
            None => return system_dll_version(&base),
        },
    };
    let pe = pe::Pe::parse(&bytes).ok()?;
    let res = pe.resources().ok()?;
    let r = res.iter().find(|r| matches!(r.kind, pe::ResId::Id(16)))?;
    pe.resource_data(r).ok().map(|d| d.to_vec())
}

/// Version resources for the system DLLs the HLE layer stands in for (Windows 7 SP1).
fn system_dll_version(name: &str) -> Option<Vec<u8>> {
    let stem = name.trim_end_matches(".dll");
    let known = ["kernel32", "user32", "gdi32", "ntdll", "advapi32", "ole32", "oleaut32", "shell32", "comctl32", "comdlg32", "winmm", "version", "d3d8", "d3d9", "ddraw", "dsound", "dinput8", "msvcrt", "shlwapi", "uxtheme", "wininet", "ws2_32", "wsock32", "imm32", "msimg32", "gdiplus", "dbghelp", "imagehlp"];
    if !known.contains(&stem) {
        return None;
    }
    // No manifest means the classic common controls (5.82).
    let ver: [u16; 4] = if stem == "comctl32" { [5, 82, 7601, 17514] } else { [6, 1, 7601, 17514] };
    Some(build_version_info(ver, &format!("{stem}.dll")))
}

fn ver_block(key: &str, ty: u16, value: &[u8], value_len: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0u8; 6];
    for u in key.encode_utf16().chain([0]) {
        b.extend_from_slice(&u.to_le_bytes());
    }
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b.extend_from_slice(value);
    for ch in children {
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.extend_from_slice(ch);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b[2..4].copy_from_slice(&value_len.to_le_bytes());
    b[4..6].copy_from_slice(&ty.to_le_bytes());
    b
}

fn ver_string(key: &str, val: &str) -> Vec<u8> {
    let mut v = Vec::new();
    let units: Vec<u16> = val.encode_utf16().chain([0]).collect();
    for u in &units {
        v.extend_from_slice(&u.to_le_bytes());
    }
    ver_block(key, 1, &v, units.len() as u16, &[])
}

pub fn build_version_info(ver: [u16; 4], name: &str) -> Vec<u8> {
    let ms = ((ver[0] as u32) << 16) | ver[1] as u32;
    let ls = ((ver[2] as u32) << 16) | ver[3] as u32;
    let mut fixed = Vec::new();
    for x in [0xFEEF_04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 2, 0, 0, 0] {
        fixed.extend_from_slice(&x.to_le_bytes());
    }
    let vs = format!("{}.{}.{}.{}", ver[0], ver[1], ver[2], ver[3]);
    let table = ver_block(
        "040904B0",
        1,
        &[],
        0,
        &[
            ver_string("CompanyName", "Microsoft Corporation"),
            ver_string("FileDescription", name),
            ver_string("FileVersion", &vs),
            ver_string("InternalName", name),
            ver_string("OriginalFilename", name),
            ver_string("ProductName", "Microsoft\u{ae} Windows\u{ae} Operating System"),
            ver_string("ProductVersion", &vs),
        ],
    );
    let sfi = ver_block("StringFileInfo", 1, &[], 0, &[table]);
    let mut tr = Vec::new();
    tr.extend_from_slice(&0x0409u16.to_le_bytes());
    tr.extend_from_slice(&0x04B0u16.to_le_bytes());
    let var = ver_block("Translation", 0, &tr, 4, &[]);
    let vfi = ver_block("VarFileInfo", 1, &[], 0, &[var]);
    ver_block("VS_VERSION_INFO", 0, &fixed, 52, &[sfi, vfi])
}

fn version_size_result(c: &mut Ctx, path: &str, n: u32, nargs: u32) -> ApiResult {
    let handle_arg = if nargs == 3 { 2 } else { 1 };
    let h = c.arg(handle_arg);
    if h != 0 {
        let _ = c.host.write_u32(h, 0);
    }
    c.trace(format!("({path:?}) = {n}"));
    if n == 0 {
        c.set_last_error(1813);
    }
    ok(nargs, n)
}

fn get_version_info(c: &mut Ctx, path: &str, len: u32, out: u32) -> ApiResult {
    match version_resource(c, path) {
        Some(d) => {
            let n = (d.len() as u32).min(len);
            let _ = c.host.write(out, &d[..n as usize]);
            ok(4, 1)
        }
        None => {
            c.set_last_error(1813);
            ok(4, 0)
        }
    }
}

/// One node of a VS_VERSIONINFO tree in guest memory.
struct VerNode {
    len: u32,
    value_len: u32,
    ty: u16,
    key: String,
    value: u32,
    children: u32,
}

fn ver_node(c: &Ctx, p: u32) -> VerNode {
    let len = c.host.read_u16(p).unwrap_or(0) as u32;
    let value_len = c.host.read_u16(p + 2).unwrap_or(0) as u32;
    let ty = c.host.read_u16(p + 4).unwrap_or(0);
    let mut key = String::new();
    let mut q = p + 6;
    loop {
        let u = c.host.read_u16(q).unwrap_or(0);
        q += 2;
        if u == 0 || key.len() > 64 {
            break;
        }
        key.push(char::from_u32(u as u32).unwrap_or('?'));
    }
    let value = (q + 3) & !3;
    let vbytes = if ty == 1 { value_len * 2 } else { value_len };
    let children = (value + vbytes + 3) & !3;
    VerNode { len, value_len, ty, key, value, children }
}

fn ver_query(c: &mut Ctx, wide: bool) -> ApiResult {
    let block = c.arg(0);
    let sub = if wide { c.wstr_arg(1) } else { c.str_arg(1) };
    let (pptr, plen) = (c.arg(2), c.arg(3));
    let mut start = block;
    let mut node = ver_node(c, block);
    for comp in sub.split('\\').filter(|s| !s.is_empty()) {
        let end = start + node.len;
        let mut p = node.children;
        let mut found = None;
        while p + 6 < end {
            let ch = ver_node(c, p);
            if ch.len == 0 {
                break;
            }
            if ch.key.eq_ignore_ascii_case(comp) {
                found = Some((p, ch));
                break;
            }
            p = (p + ch.len + 3) & !3;
        }
        match found {
            Some((p, ch)) => {
                start = p;
                node = ch;
            }
            None => {
                c.trace(format!("({sub:?}): not found"));
                return ok(4, 0);
            }
        }
    }
    let (ptr, len) = if node.ty == 1 && !wide {
        let units: Vec<u16> = (0..node.value_len).map(|i| c.host.read_u16(node.value + 2 * i).unwrap_or(0)).collect();
        let s = String::from_utf16_lossy(&units);
        let s = s.trim_end_matches('\0').to_string();
        let heap = c.p.process_heap;
        let g: &mut dyn Guest = &mut *c.host;
        let a = c.p.heap_alloc(g, heap, s.len() as u32 + 1, true);
        let _ = c.host.write_cstr(a, s.as_bytes());
        (a, s.len() as u32 + 1)
    } else {
        (node.value, node.value_len)
    };
    c.trace(format!("({sub:?}) = {ptr:#x} len {len}"));
    if pptr != 0 {
        let _ = c.host.write_u32(pptr, ptr);
    }
    if plen != 0 {
        let _ = c.host.write_u32(plen, len);
    }
    ok(4, 1)
}

fn imagelist_append(c: &mut Ctx, h: u32) -> u32 {
    match c.p.handles.get_mut(&h) {
        Some(Handle::ImageList { count, .. }) => {
            *count += 1;
            (*count - 1) as u32
        }
        _ => 0xFFFF_FFFF,
    }
}

/// Reads `n` bytes from a guest IStream (vtable slot 3: Read(this, pv, cb, pcbRead)).
fn stream_read(c: &mut Ctx, stream: u32, n: u32) -> Result<Option<Vec<u8>>, super::ApiError> {
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    let buf = c.p.heap_alloc(g, heap, n + 4, true);
    let vt = c.host.read_u32(stream).unwrap_or(0);
    let read_fn = c.host.read_u32(vt + 12).unwrap_or(0);
    let hr = c.call_guest(read_fn, &[stream, buf, n, buf + n])?;
    let got = c.host.read_u32(buf + n).unwrap_or(0);
    let mut out = vec![0u8; got.min(n) as usize];
    let _ = c.host.read(buf, &mut out);
    c.p.heap_free(buf);
    if hr != 0 || got != n {
        c.trace(format!(": IStream::Read({n}) at {read_fn:#x} = {hr:#x}, {got} bytes"));
        return Ok(None);
    }
    Ok(Some(out))
}

/// Consumes one BMP (BITMAPFILEHEADER + DIB) from a stream. ImageList_Write puts only
/// the header size in bfSize, so the length comes from the BITMAPINFOHEADER.
fn stream_skip_bmp(c: &mut Ctx, stream: u32) -> Result<bool, super::ApiError> {
    let Some(fh) = stream_read(c, stream, 14)? else { return Ok(false) };
    if &fh[..2] != b"BM" {
        c.trace(format!(": bitmap header {:02x?}", &fh[..]));
        return Ok(false);
    }
    let Some(sz) = stream_read(c, stream, 4)? else { return Ok(false) };
    let bisize = u32::from_le_bytes(sz[..4].try_into().unwrap());
    if !(12..=124).contains(&bisize) {
        return Ok(false);
    }
    let Some(rest) = stream_read(c, stream, bisize - 4)? else { return Ok(false) };
    let mut ih = sz.clone();
    ih.extend_from_slice(&rest);
    let u32at = |o: usize| u32::from_le_bytes(ih[o..o + 4].try_into().unwrap());
    let u16at = |o: usize| u16::from_le_bytes(ih[o..o + 2].try_into().unwrap()) as u32;
    let (w, h, bpp) = if bisize == 12 { (u16at(4), u16at(6), u16at(10)) } else { (u32at(4), u32at(8), u16at(14)) };
    let compression = if bisize >= 20 { u32at(16) } else { 0 };
    let size_image = if bisize >= 24 { u32at(20) } else { 0 };
    let clr_used = if bisize >= 36 { u32at(32) } else { 0 };
    let colors = if clr_used != 0 { clr_used } else if bpp <= 8 { 1 << bpp } else { 0 };
    let masks = if compression == 3 && bisize == 40 { 12 } else { 0 };
    let table = colors * if bisize == 12 { 3 } else { 4 } + masks;
    let stride = (w * bpp).div_ceil(32) * 4;
    let bits = if size_image != 0 && compression != 0 { size_image } else { stride * (h as i32).unsigned_abs() };
    Ok(stream_read(c, stream, table + bits)?.is_some())
}

/// ImageList_Read: ILHEAD, then the image bitmap and (with ILC_MASK) the mask bitmap.
fn imagelist_read(c: &mut Ctx, stream: u32) -> Result<u32, super::ApiError> {
    let Some(hd) = stream_read(c, stream, 28)? else { return Ok(0) };
    let w = |i: usize| u16::from_le_bytes([hd[i], hd[i + 1]]);
    if w(0) != 0x4C49 {
        c.trace(format!(": bad magic {:#x}", w(0)));
        return Ok(0);
    }
    let (count, cx, cy) = (w(4) as i32, w(10) as i32, w(12) as i32);
    let bk = u32::from_le_bytes(hd[14..18].try_into().unwrap());
    let flags = w(18) as u32;
    if !stream_skip_bmp(c, stream)? {
        return Ok(0);
    }
    if flags & 1 != 0 && !stream_skip_bmp(c, stream)? {
        return Ok(0);
    }
    let h = c.p.new_handle(Handle::ImageList { cx, cy, count, flags, bk });
    c.trace(format!(": {count} images {cx}x{cy} flags {flags:#x} = {h:#x}"));
    Ok(h)
}


/// Data directory `dir` of the mapped image at `base` (address, writes size).
fn image_directory(c: &mut Ctx, base: u32, dir: u32, psize: u32) -> u32 {
    let e = c.host.read_u32(base + 0x3C).unwrap_or(0);
    if c.host.read_u16(base).unwrap_or(0) != 0x5A4D || c.host.read_u32(base + e).unwrap_or(0) != 0x4550 || dir >= 16 {
        return 0;
    }
    let nt = base + e;
    let count = c.host.read_u32(nt + 0x74).unwrap_or(0);
    if dir >= count {
        return 0;
    }
    let rva = c.host.read_u32(nt + 0x78 + 8 * dir).unwrap_or(0);
    let size = c.host.read_u32(nt + 0x7C + 8 * dir).unwrap_or(0);
    if psize != 0 {
        let _ = c.host.write_u32(psize, size);
    }
    if rva == 0 { 0 } else { base + rva }
}

/// APIs only newer runners import (GameMaker Studio 2's 64-bit ones), on demand.
fn register_newer(r: &mut Registry) {
    // ETW and process settings newer runtimes set up: accepted
    // (EventSetInformation's RegHandle is 64-bit: two slots)
    r.add_on_demand("advapi32.dll", "EventSetInformation", |_c| ok(5, 0));
    r.add_on_demand("kernel32.dll", "SetProcessInformation", |_c| ok(4, 1));
    // avrt: a thread's multimedia scheduling class (GameMaker Studio 2's audio thread)
    for name in ["AvSetMmThreadCharacteristicsA", "AvSetMmThreadCharacteristicsW"] {
        r.add_on_demand("avrt.dll", name, |c| {
            let _ = c.host.write_u32(c.arg(1), 0);
            ok(2, 0xA0A0_0001)
        });
    }
    r.add_on_demand("avrt.dll", "AvRevertMmThreadCharacteristics", |_c| ok(1, 1));
    r.add_on_demand("avrt.dll", "AvSetMmThreadPriority", |_c| ok(2, 1));
    // RegGetValue(key, subkey, value, flags, pdwType, pvData, pcbData)
    r.add_on_demand("advapi32.dll", "RegGetValueA", |c| {
        let (sub, name) = (c.str_arg(1), c.str_arg(2));
        let e = get_value(c, c.arg(0), &sub, &name, c.arg(4), c.arg(5), c.arg(6));
        ok(7, e)
    });
    r.add_on_demand("advapi32.dll", "RegGetValueW", |c| {
        let (sub, name) = (c.wstr_arg(1), c.wstr_arg(2));
        let e = get_value(c, c.arg(0), &sub, &name, c.arg(4), c.arg(5), c.arg(6));
        ok(7, e)
    });
    // RegSetKeyValue(key, subkey, value, type, data, size)
    r.add_on_demand("advapi32.dll", "RegSetKeyValueA", |c| {
        let (sub, name) = (c.str_arg(1), c.str_arg(2));
        let e = set_key_value(c, c.arg(0), &sub, &name, c.arg(3), c.arg(4), c.arg(5));
        ok(6, e)
    });
    r.add_on_demand("advapi32.dll", "RegSetKeyValueW", |c| {
        let (sub, name) = (c.wstr_arg(1), c.wstr_arg(2));
        let e = set_key_value(c, c.arg(0), &sub, &name, c.arg(3), c.arg(4), c.arg(5));
        ok(6, e)
    });
    // Trimming the working set (GM8's CleanMem.dll every few frames): nothing to page out.
    r.add_on_demand("psapi.dll", "EmptyWorkingSet", |_c| ok(1, 1));
    r.add_on_demand("kernel32.dll", "K32EmptyWorkingSet", |_c| ok(1, 1));
    // No network adapters, and no notifications about them.
    let ip = "iphlpapi.dll";
    r.add_on_demand(ip, "GetAdaptersAddresses", |c| {
        let size = c.arg(4);
        if size != 0 {
            let _ = c.host.write_u32(size, 0);
        }
        ok(5, 232) // ERROR_NO_DATA
    });
    r.add_on_demand(ip, "NotifyIpInterfaceChange", |c| {
        let h = c.arg(4);
        c.write_ptr(h, 0);
        ok(5, 50) // ERROR_NOT_SUPPORTED
    });
    r.add_on_demand(ip, "CancelMibChangeNotify2", |_c| ok(1, 0));
    // UUIDs from the process's own sequence
    let rpc = "rpcrt4.dll";
    r.add_on_demand(rpc, "UuidCreate", |c| {
        let p = c.arg(0);
        c.p.next_luid += 1;
        let n = c.p.next_luid;
        let mut g = [0u8; 16];
        g[..8].copy_from_slice(&(0x6d72_6700_0000_0000u64 | n).to_le_bytes());
        g[8..].copy_from_slice(&0x4000_8000_1234_5678u64.to_le_bytes());
        g[7] = (g[7] & 0x0F) | 0x40; // version 4
        g[8] = (g[8] & 0x3F) | 0x80; // variant
        let _ = c.host.write(p, &g);
        ok(1, 0)
    });
    r.add_on_demand(rpc, "UuidToStringW", |c| {
        let (p, out) = (c.arg(0), c.arg(1));
        let mut g = [0u8; 16];
        let _ = c.host.read(p, &mut g);
        let s = super::com::format_iid(&g).trim_matches(|ch| ch == '{' || ch == '}').to_lowercase();
        let w = galloc(c, 2 * s.len() as u32 + 2);
        let _ = c.host.write_wstr(w, &s);
        c.write_ptr(out, w);
        ok(2, 0)
    });
    r.add_on_demand(rpc, "RpcStringFreeW", |c| {
        let p = c.host.read_u32(c.arg(0)).unwrap_or(0);
        if p != 0 {
            c.p.heap_free(p);
        }
        ok(1, 0)
    });
    // Media Foundation (video playback): not available
    let mfp = "mfplat.dll";
    r.add_on_demand(mfp, "MFStartup", |_c| ok(2, 0x8000_4001));
    r.add_on_demand(mfp, "MFShutdown", |_c| ok(0, 0));
    // (each fails with E_NOTIMPL, its out pointer cleared: the argument after `n` inputs)
    fn mf_fail(c: &mut Ctx, n: u32) -> ApiResult {
        let pp = c.arg(n);
        c.write_ptr(pp, 0);
        ok(n + 1, 0x8000_4001)
    }
    r.add_on_demand(mfp, "MFCreateSourceResolver", |c| mf_fail(c, 0));
    r.add_on_demand(mfp, "MFCreateMediaType", |c| mf_fail(c, 0));
    let mf = "mf.dll";
    r.add_on_demand(mf, "MFCreateAudioRendererActivate", |c| mf_fail(c, 0));
    r.add_on_demand(mf, "MFCreateMediaSession", |c| mf_fail(c, 1));
    r.add_on_demand(mf, "MFCreateSampleGrabberSinkActivate", |c| mf_fail(c, 2));
    r.add_on_demand(mf, "MFCreateTopology", |c| mf_fail(c, 0));
    r.add_on_demand(mf, "MFCreateTopologyNode", |c| mf_fail(c, 1));
    r.add_on_demand(mf, "MFGetService", |c| mf_fail(c, 3));
}

pub fn register(r: &mut Registry) {
    register_newer(r);
    // ---- advapi32 ------------------------------------------------------------
    let a = "advapi32.dll";
    r.add(a, "RegOpenKeyExA", |c| {
        let sub = c.str_arg(1);
        let out = c.arg(4);
        let e = open_key(c, c.arg(0), &sub, out);
        c.trace(format!("({sub:?})"));
        ok(5, e)
    });
    r.add(a, "RegOpenKeyExW", |c| {
        let sub = c.wstr_arg(1);
        let out = c.arg(4);
        let e = open_key(c, c.arg(0), &sub, out);
        c.trace(format!("({sub:?})"));
        ok(5, e)
    });
    r.add(a, "RegOpenKeyA", |c| {
        let sub = c.str_arg(1);
        let out = c.arg(2);
        let e = open_key(c, c.arg(0), &sub, out);
        ok(3, e)
    });
    r.add(a, "RegOpenKeyW", |c| {
        let sub = c.wstr_arg(1);
        let out = c.arg(2);
        let e = open_key(c, c.arg(0), &sub, out);
        ok(3, e)
    });
    r.add(a, "RegCreateKeyExA", |c| {
        let sub = c.str_arg(1);
        let out = c.arg(7);
        let disp = c.arg(8);
        let e = open_key(c, c.arg(0), &sub, out);
        if disp != 0 {
            let _ = c.host.write_u32(disp, 1);
        }
        ok(9, e)
    });
    r.add(a, "RegCreateKeyExW", |c| {
        let sub = c.wstr_arg(1);
        let out = c.arg(7);
        let disp = c.arg(8);
        let e = open_key(c, c.arg(0), &sub, out);
        if disp != 0 {
            let _ = c.host.write_u32(disp, 1);
        }
        ok(9, e)
    });
    r.add(a, "RegCreateKeyA", |c| {
        let sub = c.str_arg(1);
        let out = c.arg(2);
        let e = open_key(c, c.arg(0), &sub, out);
        ok(3, e)
    });
    r.add(a, "RegCreateKeyW", |c| {
        let sub = c.wstr_arg(1);
        let out = c.arg(2);
        let e = open_key(c, c.arg(0), &sub, out);
        ok(3, e)
    });
    r.add(a, "RegCloseKey", |c| {
        let h = c.arg(0);
        if root_name(h).is_none() {
            c.p.handles.remove(&h);
        }
        ok(1, ERROR_SUCCESS)
    });
    r.add(a, "RegQueryValueExA", |c| {
        let name = c.str_arg(1);
        let e = query_value(c, c.arg(0), &name, c.arg(3), c.arg(4), c.arg(5));
        ok(6, e)
    });
    r.add(a, "RegQueryValueExW", |c| {
        let name = c.wstr_arg(1);
        let e = query_value(c, c.arg(0), &name, c.arg(3), c.arg(4), c.arg(5));
        ok(6, e)
    });
    r.add(a, "RegQueryValueA", |c| {
        let sub = c.str_arg(1);
        let pcb = c.arg(3);
        let _ = sub;
        if pcb != 0 {
            let _ = c.host.write_u32(pcb, 0);
        }
        ok(4, ERROR_FILE_NOT_FOUND)
    });
    r.add(a, "RegSetValueExA", |c| {
        let name = c.str_arg(1);
        let e = set_value(c, c.arg(0), &name, c.arg(3), c.arg(4), c.arg(5));
        ok(6, e)
    });
    r.add(a, "RegSetValueExW", |c| {
        let name = c.wstr_arg(1);
        let e = set_value(c, c.arg(0), &name, c.arg(3), c.arg(4), c.arg(5));
        ok(6, e)
    });
    r.add(a, "RegDeleteKeyA", |_c| ok(2, ERROR_SUCCESS));
    r.alias(a, "RegDeleteKeyW", a, "RegDeleteKeyA");
    r.add(a, "RegDeleteValueA", |c| {
        let name = c.str_arg(1);
        if let Some(path) = key_path(c, c.arg(0)) {
            c.p.registry_values.remove(&format!("{path}\\{name}").to_lowercase());
        }
        ok(2, ERROR_SUCCESS)
    });
    r.add(a, "RegDeleteValueW", |c| {
        let name = c.wstr_arg(1);
        if let Some(path) = key_path(c, c.arg(0)) {
            c.p.registry_values.remove(&format!("{path}\\{name}").to_lowercase());
        }
        ok(2, ERROR_SUCCESS)
    });
    r.add(a, "RegEnumKeyExA", |_c| ok(8, ERROR_NO_MORE_ITEMS));
    r.alias(a, "RegEnumKeyExW", a, "RegEnumKeyExA");
    r.add(a, "RegEnumKeyA", |_c| ok(4, ERROR_NO_MORE_ITEMS));
    r.alias(a, "RegEnumKeyW", a, "RegEnumKeyA");
    r.add(a, "RegEnumValueA", |_c| ok(8, ERROR_NO_MORE_ITEMS));
    r.alias(a, "RegEnumValueW", a, "RegEnumValueA");
    r.add(a, "RegQueryInfoKeyA", |c| {
        for i in 2..12 {
            let p = c.arg(i);
            if p != 0 && i != 11 {
                let _ = c.host.write_u32(p, 0);
            }
        }
        ok(12, ERROR_SUCCESS)
    });
    r.alias(a, "RegQueryInfoKeyW", a, "RegQueryInfoKeyA");
    r.add(a, "RegFlushKey", |_c| ok(1, ERROR_SUCCESS));
    r.add(a, "RegNotifyChangeKeyValue", |_c| ok(5, ERROR_SUCCESS));
    const_apis!(r, a, {
        "RegConnectRegistryW" => (3, 53),
        "RegLoadKeyW" => (3, 5),
        "RegReplaceKeyW" => (4, 5),
        "RegRestoreKeyW" => (3, 5),
        "RegSaveKeyW" => (3, 5),
        "RegUnLoadKeyW" => (2, 5),
        "GetTraceEnableFlags" => (2, 0),
        "GetTraceEnableLevel" => (2, 0),
        "GetTraceLoggerHandle" => (1, 0),
        "RegisterTraceGuidsW" => (8, 0),
        "RegisterTraceGuidsA" => (8, 0),
        "TraceEvent" => (3, 0),
        "UnregisterTraceGuids" => (2, 0),
        "OpenProcessToken" => (3, 0),
        "OpenThreadToken" => (4, 0),
        "LookupPrivilegeValueA" => (3, 0),
        "AdjustTokenPrivileges" => (6, 0),
        "GetTokenInformation" => (5, 0),
        "CryptAcquireContextA" => (5, 0),
        "CryptAcquireContextW" => (5, 0),
        "CryptReleaseContext" => (2, 1),
        "CryptGenRandom" => (3, 0),
        "InitializeSecurityDescriptor" => (2, 1),
        "SetSecurityDescriptorDacl" => (4, 1),
        "AllocateAndInitializeSid" => (11, 0),
        "FreeSid" => (1, 0),
        "CheckTokenMembership" => (3, 0),
        "GetSecurityInfo" => (8, 5),
        "SetSecurityInfo" => (7, 5),
        "EventRegister" => (4, 0),
        "EventUnregister" => (2, 0),
        "EventWrite" => (4, 0),
        "IsTextUnicode" => (3, 0),
    });
    r.add(a, "GetUserNameA", |c| {
        let buf = c.arg(0);
        let pcb = c.arg(1);
        let cap = c.host.read_u32(pcb).unwrap_or(0);
        let n = super::kernel32_copy_str(c, buf, cap, b"player");
        let _ = c.host.write_u32(pcb, n + 1);
        ok(2, 1)
    });
    r.add(a, "GetUserNameW", |c| {
        let buf = c.arg(0);
        let pcb = c.arg(1);
        let cap = c.host.read_u32(pcb).unwrap_or(0);
        if cap >= 7 {
            let _ = c.host.write_wstr(buf, "player");
        }
        let _ = c.host.write_u32(pcb, 7);
        ok(2, (cap >= 7) as u32)
    });
    r.add(a, "SystemFunction036", |c| {
        let buf = c.arg(0);
        let len = c.arg(1);
        let mut x = 0x9E37_79B9u32 ^ c.p.clock_us as u32 ^ c.p.api_calls as u32;
        for i in 0..len {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let _ = c.host.write_u8(buf + i, x as u8);
        }
        ok(2, 1)
    });

    // ---- ole32 -------------------------------------------------------------------
    let o = "ole32.dll";
    const_apis!(r, o, {
        "CoInitialize" => (1, S_OK),
        "CoInitializeEx" => (2, S_OK),
        "CoUninitialize" => (0, 0),
        "OleInitialize" => (1, S_OK),
        "OleUninitialize" => (0, 0),
        "CoGetClassObject" => (5, REGDB_E_CLASSNOTREG),
        "CoCreateInstanceEx" => (6, REGDB_E_CLASSNOTREG),
        "CoRegisterClassObject" => (5, S_OK),
        "CoRevokeClassObject" => (1, S_OK),
        "CoFreeUnusedLibraries" => (0, 0),
        "CoDisconnectObject" => (2, S_OK),
        "CoLockObjectExternal" => (3, S_OK),
        "CoGetMalloc" => (2, E_FAIL),
        "CoRegisterMessageFilter" => (2, S_OK),
        "ProgIDFromCLSID" => (2, REGDB_E_CLASSNOTREG),
        "CLSIDFromProgID" => (2, REGDB_E_CLASSNOTREG),
        "IsAccelerator" => (4, 0),
        "OleDraw" => (4, E_NOTIMPL),
        "OleRegEnumVerbs" => (2, E_NOTIMPL),
        "OleSetMenuDescriptor" => (5, S_OK),
        "OleSetContainedObject" => (2, S_OK),
        "OleRun" => (1, S_OK),
        "OleIsRunning" => (1, 0),
        "OleFlushClipboard" => (0, S_OK),
        "OleSetClipboard" => (1, S_OK),
        "OleGetClipboard" => (1, E_FAIL),
        "OleIsCurrentClipboard" => (1, S_FALSE),
        "RegisterDragDrop" => (2, S_OK),
        "RevokeDragDrop" => (1, S_OK),
        "DoDragDrop" => (4, S_OK),
        "CreateStreamOnHGlobal" => (3, E_OUTOFMEMORY),
        "GetHGlobalFromStream" => (2, E_INVALIDARG),
        "StgOpenStorage" => (6, E_FAIL),
        "StgCreateDocfile" => (4, E_FAIL),
        "StgIsStorageFile" => (1, S_FALSE),
        "CreateBindCtx" => (2, E_FAIL),
        "OleCreateMenuDescriptor" => (2, 0),
        "OleDestroyMenuDescriptor" => (1, S_OK),
        "OleTranslateAccelerator" => (3, S_FALSE),
        "ReleaseStgMedium" => (1, 0),
        "CoWaitForMultipleHandles" => (5, E_FAIL),
        "CoGetCurrentProcess" => (0, 1),
        "CoAddRefServerProcess" => (0, 1),
        "CoReleaseServerProcess" => (0, 0),
        "CoResumeClassObjects" => (0, S_OK),
        "CoSuspendClassObjects" => (0, S_OK),
        "CoSetProxyBlanket" => (8, S_OK),
        "CoInitializeSecurity" => (9, S_OK),
        "CoMarshalInterThreadInterfaceInStream" => (3, E_FAIL),
        "CoGetInterfaceAndReleaseStream" => (3, E_FAIL),
        // no cross-apartment marshaling: callers treat the failure as "no marshaler"
        "CoCreateFreeThreadedMarshaler" => (2, E_OUTOFMEMORY),
    });
    r.add(o, "CoCreateInstance", |c| {
        let (clsid_p, iid_p, ppv) = (c.arg(0), c.arg(3), c.arg(4));
        let mut g = [0u8; 16];
        let _ = c.host.read(clsid_p, &mut g);
        let clsid = super::com::format_iid(&g);
        let made = super::wasapi::create_instance(c, &clsid, iid_p, ppv).or_else(|| super::com::create_instance(c, &clsid, iid_p, ppv));
        match made {
            Some(hr) => {
                c.trace(format!("({clsid}) = {hr:#x}"));
                ok(5, hr)
            }
            None => {
                c.p.log(format!("CoCreateInstance({clsid}): class not registered"));
                if ppv != 0 {
                    c.write_ptr(ppv, 0);
                }
                ok(5, REGDB_E_CLASSNOTREG)
            }
        }
    });
    r.add(o, "CoTaskMemAlloc", |c| {
        let n = c.arg(0);
        let p = galloc(c, n);
        ok(1, p)
    });
    r.add(o, "CoTaskMemFree", |c| {
        let p = c.arg(0);
        if p != 0 {
            c.p.heap_free(p);
        }
        ok(1, 0)
    });
    r.add(o, "CoTaskMemRealloc", |c| {
        let old = c.arg(0);
        let n = c.arg(1);
        let p = galloc(c, n);
        if old != 0 && p != 0 {
            let copy = c.p.heap_size(old).unwrap_or(0).min(n);
            let mut buf = vec![0u8; copy as usize];
            let _ = c.host.read(old, &mut buf);
            let _ = c.host.write(p, &buf);
            c.p.heap_free(old);
        }
        ok(2, p)
    });
    r.add(o, "CLSIDFromString", |c| {
        let s = c.wstr_arg(0);
        let out = c.arg(1);
        match parse_guid(&s) {
            Some(g) => {
                let _ = c.host.write(out, &g);
                ok(2, S_OK)
            }
            None => ok(2, 0x8004_01F3),
        }
    });
    r.alias(o, "IIDFromString", o, "CLSIDFromString");
    r.add(o, "StringFromCLSID", |c| {
        let p = c.arg(0);
        let out = c.arg(1);
        let mut g = [0u8; 16];
        let _ = c.host.read(p, &mut g);
        let s = format_guid(&g);
        let units: Vec<u16> = s.encode_utf16().collect();
        let mem = galloc(c, units.len() as u32 * 2 + 2);
        for (i, u) in units.iter().enumerate() {
            let _ = c.host.write_u16(mem + 2 * i as u32, *u);
        }
        let _ = c.host.write_u16(mem + 2 * units.len() as u32, 0);
        let _ = c.host.write_u32(out, mem);
        ok(2, S_OK)
    });
    r.alias(o, "StringFromIID", o, "StringFromCLSID");
    r.add(o, "StringFromGUID2", |c| {
        let p = c.arg(0);
        let out = c.arg(1);
        let cap = c.arg(2);
        let mut g = [0u8; 16];
        let _ = c.host.read(p, &mut g);
        let s = format_guid(&g);
        if cap < 39 {
            return ok(3, 0);
        }
        let _ = c.host.write_wstr(out, &s);
        ok(3, 39)
    });
    r.add(o, "IsEqualGUID", |c| {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        let _ = c.host.read(c.arg(0), &mut a);
        let _ = c.host.read(c.arg(1), &mut b);
        ok(2, (a == b) as u32)
    });
    r.add(o, "CoCreateGuid", |c| {
        let out = c.arg(0);
        let mut g = [0u8; 16];
        let mut x = 0x2545_F491u32 ^ c.p.api_calls as u32;
        for b in g.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        let _ = c.host.write(out, &g);
        ok(1, S_OK)
    });

    // ---- oleaut32 ----------------------------------------------------------------
    let oa = "oleaut32.dll";
    r.add(oa, "SysAllocStringLen", |c| {
        let src = c.arg(0);
        let len = c.arg(1);
        let units = if src == 0 { vec![0u16; len as usize] } else { read_units(c, src, len) };
        let b = bstr_alloc(c, &units);
        ok(2, b)
    });
    r.add(oa, "SysAllocString", |c| {
        let src = c.arg(0);
        if src == 0 {
            return ok(1, 0);
        }
        let units = wstr_units(c, src);
        let b = bstr_alloc(c, &units);
        ok(1, b)
    });
    r.add(oa, "SysAllocStringByteLen", |c| {
        let src = c.arg(0);
        let len = c.arg(1);
        let p = galloc(c, len + 6);
        let _ = c.host.write_u32(p, len);
        if src != 0 {
            let mut buf = vec![0u8; len as usize];
            let _ = c.host.read(src, &mut buf);
            let _ = c.host.write(p + 4, &buf);
        }
        let _ = c.host.write_u16(p + 4 + len, 0);
        ok(2, p + 4)
    });
    r.add(oa, "SysFreeString", |c| {
        let b = c.arg(0);
        bstr_free(c, b);
        ok(1, 0)
    });
    r.add(oa, "SysStringLen", |c| {
        let b = c.arg(0);
        let n = if b == 0 { 0 } else { c.host.read_u32(b - 4).unwrap_or(0) / 2 };
        ok(1, n)
    });
    r.add(oa, "SysStringByteLen", |c| {
        let b = c.arg(0);
        let n = if b == 0 { 0 } else { c.host.read_u32(b - 4).unwrap_or(0) };
        ok(1, n)
    });
    r.add(oa, "SysReAllocStringLen", |c| {
        let pb = c.arg(0);
        let src = c.arg(1);
        let len = c.arg(2);
        let old = c.host.read_u32(pb).unwrap_or(0);
        let units = if src == 0 {
            let mut u = if old == 0 { Vec::new() } else { wstr_units(c, old) };
            u.resize(len as usize, 0);
            u
        } else {
            read_units(c, src, len)
        };
        let b = bstr_alloc(c, &units);
        bstr_free(c, old);
        let _ = c.host.write_u32(pb, b);
        ok(3, (b != 0) as u32)
    });
    r.add(oa, "SysReAllocString", |c| {
        let pb = c.arg(0);
        let src = c.arg(1);
        let old = c.host.read_u32(pb).unwrap_or(0);
        let units = if src == 0 { Vec::new() } else { wstr_units(c, src) };
        let b = bstr_alloc(c, &units);
        bstr_free(c, old);
        let _ = c.host.write_u32(pb, b);
        ok(2, (b != 0) as u32)
    });
    r.add(oa, "VariantInit", |c| {
        let p = c.arg(0);
        let _ = c.host.fill(p, 16, 0);
        ok(1, 0)
    });
    r.add(oa, "VariantClear", |c| {
        let p = c.arg(0);
        let vt = c.host.read_u16(p).unwrap_or(0);
        if vt == 8 {
            let b = c.host.read_u32(p + 8).unwrap_or(0);
            bstr_free(c, b);
        }
        let _ = c.host.fill(p, 16, 0);
        ok(1, S_OK)
    });
    r.add(oa, "VariantCopy", |c| {
        let dst = c.arg(0);
        let src = c.arg(1);
        let mut buf = [0u8; 16];
        let _ = c.host.read(src, &mut buf);
        let vt = u16::from_le_bytes([buf[0], buf[1]]);
        if vt == 8 {
            let b = u32::from_le_bytes(buf[8..12].try_into().unwrap());
            let units = if b == 0 { Vec::new() } else { wstr_units(c, b) };
            let nb = bstr_alloc(c, &units);
            buf[8..12].copy_from_slice(&nb.to_le_bytes());
        }
        let _ = c.host.write(dst, &buf);
        ok(2, S_OK)
    });
    r.alias(oa, "VariantCopyInd", oa, "VariantCopy");
    r.add(oa, "VariantChangeType", |c| {
        let vt = c.arg(3);
        c.trace(format!(": to vt {vt} not supported"));
        ok(4, 0x8002_0005)
    });
    r.alias(oa, "VariantChangeTypeEx", oa, "VariantChangeType");
    const_apis!(r, oa, {
        "SafeArrayCreate" => (3, 0),
        "SafeArrayDestroy" => (1, S_OK),
        "SafeArrayGetLBound" => (3, E_INVALIDARG),
        "SafeArrayGetUBound" => (3, E_INVALIDARG),
        "SafeArrayPtrOfIndex" => (3, E_INVALIDARG),
        "SafeArrayAccessData" => (2, E_INVALIDARG),
        "SafeArrayUnaccessData" => (1, E_INVALIDARG),
        "SafeArrayGetDim" => (1, 0),
        "SafeArrayGetElemsize" => (1, 0),
        "SafeArrayRedim" => (2, E_INVALIDARG),
        "SafeArrayCopy" => (2, E_INVALIDARG),
        "SafeArrayGetElement" => (3, E_INVALIDARG),
        "SafeArrayPutElement" => (3, E_INVALIDARG),
        "SafeArrayGetVartype" => (2, E_INVALIDARG),
        "GetActiveObject" => (3, 0x8004_01E3),
        "GetErrorInfo" => (2, S_FALSE),
        "SetErrorInfo" => (2, S_OK),
        "CreateErrorInfo" => (1, E_FAIL),
        "OleCreateFontIndirect" => (3, E_NOTIMPL),
        "OleCreatePictureIndirect" => (4, E_NOTIMPL),
        "OleCreatePropertyFrame" => (11, E_NOTIMPL),
        "OleLoadPicture" => (5, E_NOTIMPL),
        "OleLoadPictureEx" => (8, E_NOTIMPL),
        "OleSavePictureFile" => (2, E_NOTIMPL),
        "LoadTypeLib" => (2, E_FAIL),
        "LoadTypeLibEx" => (3, E_FAIL),
        "RegisterTypeLib" => (3, E_FAIL),
        "UnRegisterTypeLib" => (5, E_FAIL),
        "DispGetParam" => (5, E_FAIL),
        "DispInvoke" => (8, E_FAIL),
        "VarBstrCat" => (3, E_NOTIMPL),
        "VarBstrCmp" => (4, E_NOTIMPL),
        "VarR8FromStr" => (4, E_NOTIMPL),
        "VarDateFromStr" => (4, E_NOTIMPL),
        "VariantTimeToSystemTime" => (2, 0),
        "SystemTimeToVariantTime" => (2, 0),
        "VarUI4FromStr" => (4, E_NOTIMPL),
        "OaBuildVersion" => (0, 0x0002_0000),
    });
    for (ord, name) in [(2, "SysAllocString"), (3, "SysReAllocString"), (4, "SysAllocStringLen"), (5, "SysReAllocStringLen"), (6, "SysFreeString"), (7, "SysStringLen"), (8, "VariantInit"), (9, "VariantClear"), (10, "VariantCopy"), (11, "VariantCopyInd"), (12, "VariantChangeType"), (15, "SafeArrayCreate"), (16, "SafeArrayDestroy"), (17, "SafeArrayGetDim"), (18, "SafeArrayGetElemsize"), (19, "SafeArrayGetUBound"), (20, "SafeArrayGetLBound"), (23, "SafeArrayAccessData"), (24, "SafeArrayUnaccessData"), (25, "SafeArrayGetElement"), (26, "SafeArrayPutElement"), (27, "SafeArrayCopy"), (28, "SafeArrayPtrOfIndex"), (77, "SafeArrayGetVartype"), (149, "SysStringByteLen"), (150, "SysAllocStringByteLen"), (161, "LoadTypeLib"), (183, "LoadTypeLibEx"), (184, "VariantChangeTypeEx"), (200, "GetErrorInfo"), (201, "SetErrorInfo"), (202, "CreateErrorInfo"), (277, "OaBuildVersion"), (406, "OleCreateFontIndirect"), (417, "OleLoadPicture"), (420, "OleCreatePropertyFrame")] {
        r.alias(oa, &format!("#{ord}"), oa, name);
    }
    for name in ["OleCreateFontIndirect", "OleCreatePictureIndirect", "OleCreatePropertyFrame", "OleLoadPicture"] {
        r.alias("olepro32.dll", name, oa, name);
    }

    // ---- shell32 -------------------------------------------------------------------
    let s = "shell32.dll";
    r.add(s, "SHGetFolderPathW", |c| {
        let csidl = c.arg(1);
        let out = c.arg(4);
        match csidl_path(csidl) {
            Some(p) => {
                let _ = c.host.write_wstr(out, p);
                c.trace(format!("({csidl:#x}) = {p:?}"));
                ok(5, S_OK)
            }
            None => ok(5, E_INVALIDARG),
        }
    });
    r.add(s, "SHGetFolderPathA", |c| {
        let csidl = c.arg(1);
        let out = c.arg(4);
        match csidl_path(csidl) {
            Some(p) => {
                let _ = c.host.write_cstr(out, p.as_bytes());
                ok(5, S_OK)
            }
            None => ok(5, E_INVALIDARG),
        }
    });
    r.add(s, "SHGetSpecialFolderPathA", |c| {
        let out = c.arg(1);
        let csidl = c.arg(2);
        match csidl_path(csidl) {
            Some(p) => {
                let _ = c.host.write_cstr(out, p.as_bytes());
                ok(4, 1)
            }
            None => ok(4, 0),
        }
    });
    r.add(s, "SHGetSpecialFolderPathW", |c| {
        let out = c.arg(1);
        let csidl = c.arg(2);
        match csidl_path(csidl) {
            Some(p) => {
                let _ = c.host.write_wstr(out, p);
                ok(4, 1)
            }
            None => ok(4, 0),
        }
    });
    r.add(s, "ShellExecuteA", |c| {
        let file = c.str_arg(2);
        c.p.log(format!("ShellExecuteA({file:?}) ignored"));
        ok(6, 42)
    });
    r.add(s, "ShellExecuteW", |c| {
        let file = c.wstr_arg(2);
        c.p.log(format!("ShellExecuteW({file:?}) ignored"));
        ok(6, 42)
    });
    const_apis!(r, s, {
        "ShellExecuteExA" => (1, 1),
        "ShellExecuteExW" => (1, 1),
        "SHGetMalloc" => (1, E_FAIL),
        "SHGetDesktopFolder" => (1, E_FAIL),
        "SHBrowseForFolderA" => (1, 0),
        "SHBrowseForFolderW" => (1, 0),
        "SHGetPathFromIDListA" => (2, 0),
        "SHGetPathFromIDListW" => (2, 0),
        "SHGetSpecialFolderLocation" => (3, E_FAIL),
        "SHCreateItemFromParsingName" => (4, E_NOTIMPL),
        "SHFileOperationA" => (1, 0),
        "SHFileOperationW" => (1, 0),
        "SHGetFileInfoA" => (5, 0),
        "SHGetFileInfoW" => (5, 0),
        "SHAppBarMessage" => (2, 0),
        "Shell_NotifyIconA" => (2, 1),
        "Shell_NotifyIconW" => (2, 1),
        "DragAcceptFiles" => (2, 0),
        "DragQueryFileA" => (4, 0),
        "DragQueryFileW" => (4, 0),
        "DragQueryPoint" => (2, 0),
        "DragFinish" => (1, 0),
        "ExtractIconA" => (3, 0),
        "ExtractIconW" => (3, 0),
        "ExtractIconExA" => (5, 0),
        "ExtractIconExW" => (5, 0),
        "ExtractAssociatedIconA" => (3, 0),
        "SHChangeNotify" => (4, 0),
        "SHGetKnownFolderPath" => (4, E_FAIL),
        "SHEmptyRecycleBinA" => (3, S_OK),
        "FindExecutableA" => (3, 2),
        "CommandLineToArgvW" => (2, 0),
        "SHGetInstanceExplorer" => (1, E_FAIL),
        "ILFree" => (1, 0),
        "SHParseDisplayName" => (5, E_FAIL),
        "SHCreateDirectoryExW" => (3, 0),
    });

    // ---- version -------------------------------------------------------------------
    let v = "version.dll";
    r.add(v, "GetFileVersionInfoSizeW", |c| {
        let path = c.wstr_arg(0);
        let n = version_resource(c, &path).map(|d| d.len() as u32 * 2).unwrap_or(0);
        version_size_result(c, &path, n, 2)
    });
    r.add(v, "GetFileVersionInfoSizeA", |c| {
        let path = c.str_arg(0);
        let n = version_resource(c, &path).map(|d| d.len() as u32 * 2).unwrap_or(0);
        version_size_result(c, &path, n, 2)
    });
    r.add(v, "GetFileVersionInfoW", |c| {
        let path = c.wstr_arg(0);
        let (len, out) = (c.arg(2), c.arg(3));
        get_version_info(c, &path, len, out)
    });
    r.add(v, "GetFileVersionInfoA", |c| {
        let path = c.str_arg(0);
        let (len, out) = (c.arg(2), c.arg(3));
        get_version_info(c, &path, len, out)
    });
    r.add(v, "GetFileVersionInfoSizeExW", |c| {
        let path = c.wstr_arg(1);
        let n = version_resource(c, &path).map(|d| d.len() as u32 * 2).unwrap_or(0);
        version_size_result(c, &path, n, 3)
    });
    r.add(v, "GetFileVersionInfoExW", |c| {
        let path = c.wstr_arg(1);
        let (len, out) = (c.arg(3), c.arg(4));
        let r = get_version_info(c, &path, len, out)?;
        Ok(Ret::stdcall(5, r.eax))
    });
    r.add(v, "VerQueryValueW", |c| ver_query(c, true));
    r.add(v, "VerQueryValueA", |c| ver_query(c, false));
    const_apis!(r, v, {
        "VerLanguageNameA" => (3, 0),
        "VerFindFileA" => (8, 0),
        "VerInstallFileA" => (8, 0),
    });

    // ---- winmm -----------------------------------------------------------------------
    let w = "winmm.dll";
    r.add(w, "timeGetTime", |c| {
        c.poll_clock();
        ok(0, c.p.now_ms())
    });
    r.add(w, "timeBeginPeriod", |_c| ok(1, 0));
    r.add(w, "timeEndPeriod", |_c| ok(1, 0));
    r.add(w, "timeGetDevCaps", |c| {
        let out = c.arg(0);
        let _ = c.host.write_u32(out, 1);
        let _ = c.host.write_u32(out + 4, 1_000_000);
        ok(2, 0)
    });
    r.add(w, "timeGetSystemTime", |c| {
        let out = c.arg(0);
        let _ = c.host.write_u32(out, 1);
        let _ = c.host.write_u32(out + 4, c.p.now_ms());
        ok(2, 0)
    });
    r.add(w, "timeSetEvent", |c| {
        c.p.log("timeSetEvent: multimedia timers are not implemented".into());
        ok(5, 0)
    });
    r.add(w, "timeKillEvent", |_c| ok(1, 0));
    const_apis!(r, w, {
        "joyGetNumDevs" => (0, 0),
        "joyGetDevCapsA" => (3, 165),
        "joyGetDevCapsW" => (3, 165),
        "joyGetPos" => (2, 167),
        "joyGetPosEx" => (2, 167),
        "joySetCapture" => (4, 165),
        "joyReleaseCapture" => (1, 165),
        "mciGetErrorStringA" => (3, 0),
        "mciGetErrorStringW" => (3, 0),
        "sndPlaySoundA" => (2, 1),
        "sndPlaySoundW" => (2, 1),
        "PlaySoundA" => (3, 1),
        "PlaySoundW" => (3, 1),
        "waveInGetNumDevs" => (0, 0),
        "waveInGetDevCapsA" => (3, 6),
        "waveInGetDevCapsW" => (3, 6),
        "waveInOpen" => (6, 6),
        "waveInClose" => (1, 6),
        "waveInAddBuffer" => (3, 6),
        "waveInPrepareHeader" => (3, 6),
        "waveInUnprepareHeader" => (3, 6),
        "waveInReset" => (1, 6),
        "waveInStart" => (1, 6),
        "waveInStop" => (1, 6),
        "waveOutGetNumDevs" => (0, 0),
        "waveOutGetDevCapsA" => (3, 6),
        "waveOutGetDevCapsW" => (3, 6),
        "waveOutOpen" => (6, 6),
        "waveOutClose" => (1, 6),
        "waveOutWrite" => (3, 6),
        "waveOutPrepareHeader" => (3, 6),
        "waveOutUnprepareHeader" => (3, 6),
        "waveOutReset" => (1, 6),
        "waveOutPause" => (1, 6),
        "waveOutRestart" => (1, 6),
        "waveOutGetPosition" => (3, 6),
        "waveOutGetVolume" => (2, 6),
        "waveOutSetVolume" => (2, 6),
        "waveOutGetErrorTextA" => (3, 6),
        "midiOutGetNumDevs" => (0, 0),
        "midiOutOpen" => (5, 6),
        "midiOutClose" => (1, 6),
        "midiOutShortMsg" => (2, 6),
        "midiOutReset" => (1, 6),
        "midiOutGetDevCapsA" => (3, 6),
        "midiStreamOpen" => (6, 6),
        "midiInGetNumDevs" => (0, 0),
        "mixerGetNumDevs" => (0, 0),
        "mixerOpen" => (5, 6),
        "mixerClose" => (1, 6),
        "mixerGetLineInfoA" => (3, 6),
        "mixerGetLineControlsA" => (3, 6),
        "mixerGetControlDetailsA" => (3, 6),
        "mixerSetControlDetails" => (3, 6),
        "auxGetNumDevs" => (0, 0),
        "auxGetVolume" => (2, 6),
        "auxSetVolume" => (2, 6),
        "mmioOpenA" => (3, 0),
        "mmioClose" => (2, 0),
        "mmioRead" => (3, 0xFFFF_FFFF),
        "mmioDescend" => (4, 265),
        "mmioAscend" => (3, 0),
        "mmioStringToFOURCCA" => (2, 0),
    });

    r.add(w, "mciSendCommandA", |c| super::mci::command(c, false));
    r.add(w, "mciSendCommandW", |c| super::mci::command(c, true));
    r.add(w, "mciSendStringA", |c| {
        let cmd = c.str_arg(0);
        let r = super::mci::string(c, &cmd, c.arg(1), c.arg(2), c.arg(3), false);
        ok(4, r)
    });
    r.add(w, "mciSendStringW", |c| {
        let cmd = c.wstr_arg(0);
        let r = super::mci::string(c, &cmd, c.arg(1), c.arg(2), c.arg(3), true);
        ok(4, r)
    });
    r.add(w, "mciGetDeviceIDA", |c| {
        let name = c.str_arg(0);
        let id = super::mci::device_id(c, &name);
        ok(1, id)
    });
    r.add(w, "mciGetDeviceIDW", |c| {
        let name = c.wstr_arg(0);
        let id = super::mci::device_id(c, &name);
        ok(1, id)
    });

    // ---- imm32 -------------------------------------------------------------------------
    const_apis!(r, "imm32.dll", {
        "ImmGetContext" => (1, 0),
        "ImmReleaseContext" => (2, 1),
        "ImmGetConversionStatus" => (3, 0),
        "ImmSetConversionStatus" => (3, 0),
        "ImmIsIME" => (1, 0),
        "ImmNotifyIME" => (4, 0),
        "ImmGetCompositionStringA" => (4, 0),
        "ImmGetCompositionStringW" => (4, 0),
        "ImmSetCompositionFontA" => (2, 0),
        "ImmSetCompositionFontW" => (2, 0),
        "ImmSetCompositionWindow" => (2, 0),
        "ImmSetOpenStatus" => (2, 0),
        "ImmGetOpenStatus" => (1, 0),
        "ImmAssociateContext" => (2, 0),
        "ImmGetDefaultIMEWnd" => (1, 0),
        "ImmDisableIME" => (1, 1),
        "ImmGetCandidateWindow" => (3, 0),
        "ImmSetCandidateWindow" => (2, 0),
        "ImmGetIMEFileNameA" => (3, 0),
        "ImmGetProperty" => (2, 0),
        "ImmGetVirtualKey" => (1, 0),
        "ImmSimulateHotKey" => (2, 0),
        "ImmCreateContext" => (0, 0),
        "ImmDestroyContext" => (1, 0),
        "ImmEscapeA" => (4, 0),
        "ImmGetCompositionWindow" => (2, 0),
    });

    // ---- comctl32 ---------------------------------------------------------------------
    let cc = "comctl32.dll";
    r.add(cc, "InitCommonControls", |_c| ok(0, 0));
    r.add(cc, "InitCommonControlsEx", |_c| ok(1, 1));
    r.alias(cc, "#17", cc, "InitCommonControls");
    r.add(cc, "ImageList_Create", |c| {
        let (cx, cy, flags) = (c.arg_i32(0), c.arg_i32(1), c.arg(2));
        let h = c.p.new_handle(Handle::ImageList { cx, cy, count: 0, flags, bk: 0xFFFF_FFFF });
        ok(5, h)
    });
    r.add(cc, "ImageList_Destroy", |c| {
        let h = c.arg(0);
        let r = matches!(c.p.handles.remove(&h), Some(Handle::ImageList { .. }));
        ok(1, r as u32)
    });
    r.add(cc, "ImageList_GetIconSize", |c| {
        let (h, pcx, pcy) = (c.arg(0), c.arg(1), c.arg(2));
        let Some(Handle::ImageList { cx, cy, .. }) = c.p.handles.get(&h).cloned() else { return ok(3, 0) };
        let _ = c.host.write_u32(pcx, cx as u32);
        let _ = c.host.write_u32(pcy, cy as u32);
        ok(3, 1)
    });
    r.add(cc, "ImageList_SetIconSize", |c| {
        let (h, ncx, ncy) = (c.arg(0), c.arg_i32(1), c.arg_i32(2));
        match c.p.handles.get_mut(&h) {
            Some(Handle::ImageList { cx, cy, count, .. }) => {
                *cx = ncx;
                *cy = ncy;
                *count = 0;
                ok(3, 1)
            }
            _ => ok(3, 0),
        }
    });
    r.add(cc, "ImageList_GetImageCount", |c| {
        let h = c.arg(0);
        let n = match c.p.handles.get(&h) {
            Some(Handle::ImageList { count, .. }) => *count as u32,
            _ => 0,
        };
        ok(1, n)
    });
    r.add(cc, "ImageList_SetImageCount", |c| {
        let (h, n) = (c.arg(0), c.arg_i32(1));
        match c.p.handles.get_mut(&h) {
            Some(Handle::ImageList { count, .. }) => {
                *count = n;
                ok(2, 1)
            }
            _ => ok(2, 0),
        }
    });
    for (name, nargs) in [("ImageList_Add", 3u32), ("ImageList_AddMasked", 3), ("ImageList_AddIcon", 2)] {
        let f: super::ApiFn = match nargs {
            3 => |c| {
                let h = c.arg(0);
                let idx = imagelist_append(c, h);
                ok(3, idx)
            },
            _ => |c| {
                let h = c.arg(0);
                let idx = imagelist_append(c, h);
                ok(2, idx)
            },
        };
        r.add(cc, name, f);
    }
    r.add(cc, "ImageList_ReplaceIcon", |c| {
        let (h, i) = (c.arg(0), c.arg_i32(1));
        let idx = if i < 0 { imagelist_append(c, h) } else { i as u32 };
        ok(3, idx)
    });
    r.add(cc, "ImageList_Remove", |c| {
        let (h, i) = (c.arg(0), c.arg_i32(1));
        match c.p.handles.get_mut(&h) {
            Some(Handle::ImageList { count, .. }) => {
                if i < 0 {
                    *count = 0;
                } else if i < *count {
                    *count -= 1;
                }
                ok(2, 1)
            }
            _ => ok(2, 0),
        }
    });
    r.add(cc, "ImageList_GetBkColor", |c| {
        let h = c.arg(0);
        let v = match c.p.handles.get(&h) {
            Some(Handle::ImageList { bk, .. }) => *bk,
            _ => 0xFFFF_FFFF,
        };
        ok(1, v)
    });
    r.add(cc, "ImageList_SetBkColor", |c| {
        let (h, v) = (c.arg(0), c.arg(1));
        let prev = match c.p.handles.get_mut(&h) {
            Some(Handle::ImageList { bk, .. }) => std::mem::replace(bk, v),
            _ => 0xFFFF_FFFF,
        };
        ok(2, prev)
    });
    r.add(cc, "ImageList_Read", |c| {
        let stream = c.arg(0);
        let h = imagelist_read(c, stream)?;
        ok(1, h)
    });
    r.add(cc, "ImageList_ReadEx", |c| {
        let (stream, ppv) = (c.arg(1), c.arg(3));
        let h = imagelist_read(c, stream)?;
        if ppv != 0 {
            let _ = c.host.write_u32(ppv, h);
        }
        ok(4, if h != 0 { S_OK } else { E_FAIL })
    });
    r.add(cc, "ImageList_Write", |c| {
        c.p.log("ImageList_Write: not supported".into());
        ok(2, 0)
    });
    r.add(cc, "ImageList_Duplicate", |c| {
        let h = c.arg(0);
        let v = match c.p.handles.get(&h).cloned() {
            Some(il @ Handle::ImageList { .. }) => c.p.new_handle(il),
            _ => 0,
        };
        ok(1, v)
    });
    const_apis!(r, cc, {
        "ImageList_Copy" => (5, 1),
        "ImageList_BeginDrag" => (4, 0),
        "ImageList_EndDrag" => (0, 0),
        "ImageList_DragEnter" => (3, 0),
        "ImageList_DragLeave" => (1, 0),
        "ImageList_DragMove" => (2, 0),
        "ImageList_DragShowNolock" => (1, 0),
        "ImageList_Draw" => (6, 1),
        "ImageList_DrawEx" => (10, 1),
        "ImageList_DrawIndirect" => (1, 1),
        "ImageList_GetDragImage" => (2, 0),
        "ImageList_GetIcon" => (3, 0),
        "ImageList_GetImageInfo" => (3, 0),
        "ImageList_LoadImageA" => (7, 0),
        "ImageList_LoadImageW" => (7, 0),
        "ImageList_Replace" => (4, 1),
        "ImageList_SetOverlayImage" => (3, 1),
        "ImageList_Merge" => (6, 0),
        "ImageList_SetDragCursorImage" => (4, 0),
        "FlatSB_GetScrollInfo" => (3, 0),
        "FlatSB_GetScrollPos" => (2, 0),
        "FlatSB_SetScrollInfo" => (4, 0),
        "FlatSB_SetScrollPos" => (4, 0),
        "FlatSB_SetScrollProp" => (4, 1),
        "FlatSB_GetScrollProp" => (3, 0),
        "FlatSB_ShowScrollBar" => (3, 0),
        "FlatSB_EnableScrollBar" => (3, 0),
        "InitializeFlatSB" => (1, 1),
        "UninitializeFlatSB" => (1, 0),
        "_TrackMouseEvent" => (1, 1),
        "CreateToolbarEx" => (13, 0),
        "CreateStatusWindowA" => (4, 0),
        "CreatePropertySheetPageA" => (1, 0),
        "PropertySheetA" => (1, 0xFFFF_FFFF),
        "MakeDragList" => (1, 0),
        "LBItemFromPt" => (4, 0xFFFF_FFFF),
        "DrawInsert" => (3, 0),
        "CreateUpDownControl" => (12, 0),
        "DrawStatusTextA" => (4, 0),
        "DPA_Create" => (1, 0),
        "DSA_Create" => (2, 0),
        "GetEffectiveClientRect" => (3, 0),
        "ShowHideMenuCtl" => (3, 0),
        "InitMUILanguage" => (1, 0),
        "GetMUILanguage" => (0, 0),
        "DefSubclassProc" => (4, 0),
        "SetWindowSubclass" => (4, 0),
        "RemoveWindowSubclass" => (3, 0),
        "TaskDialogIndirect" => (4, E_NOTIMPL),
        "TaskDialog" => (8, E_NOTIMPL),
    });

    // ---- comdlg32 ----------------------------------------------------------------------
    const_apis!(r, "comdlg32.dll", {
        "ChooseColorA" => (1, 0),
        "ChooseColorW" => (1, 0),
        "ChooseFontA" => (1, 0),
        "ChooseFontW" => (1, 0),
        "GetOpenFileNameA" => (1, 0),
        "GetOpenFileNameW" => (1, 0),
        "GetSaveFileNameA" => (1, 0),
        "GetSaveFileNameW" => (1, 0),
        "PrintDlgA" => (1, 0),
        "PrintDlgW" => (1, 0),
        "PrintDlgExA" => (1, E_FAIL),
        "PrintDlgExW" => (1, E_FAIL),
        "PageSetupDlgA" => (1, 0),
        "FindTextA" => (1, 0),
        "ReplaceTextA" => (1, 0),
        "CommDlgExtendedError" => (0, 0),
    });

    // ---- small libraries ---------------------------------------------------------------
    const_apis!(r, "crypt32.dll", {
        "CryptQueryObject" => (11, 0),
        "CryptProtectData" => (7, 0),
        "CryptUnprotectData" => (7, 0),
        "CertOpenStore" => (5, 0),
        "CertCloseStore" => (2, 1),
    });
    const_apis!(r, "uxtheme.dll", {
        "IsThemeActive" => (0, 0),
        "IsAppThemed" => (0, 0),
        "OpenThemeData" => (2, 0),
        "CloseThemeData" => (1, S_OK),
        "DrawThemeBackground" => (6, E_FAIL),
        "DrawThemeText" => (9, E_FAIL),
        "DrawThemeEdge" => (8, E_FAIL),
        "DrawThemeIcon" => (7, E_FAIL),
        "DrawThemeParentBackground" => (3, S_OK),
        "GetThemeBackgroundContentRect" => (6, E_FAIL),
        "GetThemeBackgroundRegion" => (6, E_FAIL),
        "GetThemeColor" => (5, E_FAIL),
        "GetThemeFont" => (6, E_FAIL),
        "GetThemeInt" => (5, E_FAIL),
        "GetThemeMargins" => (7, E_FAIL),
        "GetThemePartSize" => (7, E_FAIL),
        "GetThemeSysColor" => (2, 0),
        "GetThemeSysFont" => (3, E_FAIL),
        "GetThemeSysSize" => (2, 0),
        "GetThemeTextExtent" => (8, E_FAIL),
        "GetThemeTextMetrics" => (5, E_FAIL),
        "GetCurrentThemeName" => (6, E_FAIL),
        "GetThemeDocumentationProperty" => (4, E_FAIL),
        "GetThemeAppProperties" => (0, 0),
        "SetThemeAppProperties" => (1, 0),
        "SetWindowTheme" => (3, S_OK),
        "IsThemePartDefined" => (3, 0),
        "IsThemeBackgroundPartiallyTransparent" => (3, 0),
        "HitTestThemeBackground" => (9, E_FAIL),
        "EnableThemeDialogTexture" => (2, S_OK),
        "EnableTheming" => (1, S_OK),
        "GetThemeBool" => (5, E_FAIL),
        "GetThemeEnumValue" => (5, E_FAIL),
        "GetThemePosition" => (5, E_FAIL),
        "GetThemeRect" => (5, E_FAIL),
        "GetThemeString" => (6, E_FAIL),
        "GetThemeFilename" => (6, E_FAIL),
        "GetThemePropertyOrigin" => (5, E_FAIL),
        "GetThemeMetric" => (6, E_FAIL),
        "DrawThemeTextEx" => (9, E_FAIL),
        "BufferedPaintInit" => (0, S_OK),
        "BufferedPaintSetAlpha" => (3, S_OK),
        "BufferedPaintUnInit" => (0, S_OK),
        "BeginBufferedPaint" => (5, 0),
        "EndBufferedPaint" => (2, S_OK),
        "IsCompositionActive" => (0, 0),
        "GetThemeTransitionDuration" => (6, E_FAIL),
    });
    let dwm = "dwmapi.dll";
    r.add(dwm, "DwmIsCompositionEnabled", |c| {
        let out = c.arg(0);
        let _ = c.host.write_u32(out, 0);
        ok(1, S_OK)
    });
    const_apis!(r, dwm, {
        "DwmEnableMMCSS" => (1, S_OK),
        "DwmExtendFrameIntoClientArea" => (2, E_NOTIMPL),
        "DwmGetWindowAttribute" => (4, E_NOTIMPL),
        "DwmSetWindowAttribute" => (4, E_NOTIMPL),
        "DwmEnableComposition" => (1, S_OK),
        "DwmEnableBlurBehindWindow" => (2, E_NOTIMPL),
        "DwmGetColorizationColor" => (2, E_NOTIMPL),
        "DwmFlush" => (0, S_OK),
        "DwmSetPresentParameters" => (2, E_NOTIMPL),
        "DwmDefWindowProc" => (5, 0),
    });
    const_apis!(r, "msimg32.dll", {
        "AlphaBlend" => (11, 1),
        "TransparentBlt" => (11, 1),
        "GradientFill" => (6, 1),
        "DllInitialize" => (3, 1),
        "vSetDdrawflag" => (0, 0),
    });
    const_apis!(r, "gdiplus.dll", {
        "GdiplusShutdown" => (1, 0),
        "GdipCreateFromHDC" => (2, 1),
        "GdipDeleteGraphics" => (1, 1),
    });
    r.add("gdiplus.dll", "GdiplusStartup", |c| {
        let token = c.arg(0);
        let output = c.arg(2);
        c.write_ptr(token, 1);
        if output != 0 {
            // (two function pointers)
            let _ = c.host.write_u64(output, 0);
            if c.x64() {
                let _ = c.host.write_u64(output + 8, 0);
            }
        }
        ok(3, 0)
    });
    const_apis!(r, "windowscodecs.dll", {
        "WICConvertBitmapSource" => (3, E_NOTIMPL),
        "WICCreateImagingFactory_Proxy" => (2, E_NOTIMPL),
    });
    const_apis!(r, "ws2_32.dll", {
        "WSAStartup" => (2, 10091),
        "WSACleanup" => (0, 0),
        "WSAGetLastError" => (0, 10093),
        "WSASetLastError" => (1, 0),
        "socket" => (3, 0xFFFF_FFFF),
        "closesocket" => (1, 0xFFFF_FFFF),
        "connect" => (3, 0xFFFF_FFFF),
        "bind" => (3, 0xFFFF_FFFF),
        "listen" => (2, 0xFFFF_FFFF),
        "accept" => (3, 0xFFFF_FFFF),
        "send" => (4, 0xFFFF_FFFF),
        "sendto" => (6, 0xFFFF_FFFF),
        "recv" => (4, 0xFFFF_FFFF),
        "recvfrom" => (6, 0xFFFF_FFFF),
        "select" => (5, 0xFFFF_FFFF),
        "ioctlsocket" => (3, 0xFFFF_FFFF),
        "setsockopt" => (5, 0xFFFF_FFFF),
        "getsockopt" => (5, 0xFFFF_FFFF),
        "getsockname" => (3, 0xFFFF_FFFF),
        "getpeername" => (3, 0xFFFF_FFFF),
        "shutdown" => (2, 0xFFFF_FFFF),
        "gethostname" => (2, 0xFFFF_FFFF),
        "gethostbyname" => (1, 0),
        "gethostbyaddr" => (3, 0),
        "getaddrinfo" => (4, 11001),
        "freeaddrinfo" => (1, 0),
        "inet_addr" => (1, 0xFFFF_FFFF),
        "inet_ntoa" => (1, 0),
        "WSAAsyncSelect" => (4, 0xFFFF_FFFF),
        "WSAEventSelect" => (3, 0xFFFF_FFFF),
        "WSACreateEvent" => (0, 0),
        "WSACloseEvent" => (1, 0),
        "WSAWaitForMultipleEvents" => (5, 0xFFFF_FFFF),
        "WSAEnumNetworkEvents" => (3, 0xFFFF_FFFF),
        "WSAIoctl" => (9, 0xFFFF_FFFF),
        "WSASocketA" => (6, 0xFFFF_FFFF),
        "WSASocketW" => (6, 0xFFFF_FFFF),
        "__WSAFDIsSet" => (2, 0),
        "WSARecv" => (7, 0xFFFF_FFFF),
        "WSASend" => (7, 0xFFFF_FFFF),
        "WSAResetEvent" => (1, 0),
        "WSASetEvent" => (1, 0),
        "WSAGetOverlappedResult" => (5, 0),
        "WSAAddressToStringA" => (5, 0xFFFF_FFFF),
        "WSAStringToAddressA" => (5, 0xFFFF_FFFF),
        "getservbyname" => (2, 0),
        "getprotobyname" => (1, 0),
    });
    // byte order is arithmetic, not networking
    r.add("ws2_32.dll", "htons", |c| ok(1, (c.arg(0) as u16).swap_bytes() as u32));
    r.add("ws2_32.dll", "ntohs", |c| ok(1, (c.arg(0) as u16).swap_bytes() as u32));
    r.add("ws2_32.dll", "htonl", |c| ok(1, c.arg(0).swap_bytes()));
    r.add("ws2_32.dll", "ntohl", |c| ok(1, c.arg(0).swap_bytes()));
    // Imports by ordinal (the numbers are the same in wsock32 for these).
    for (ord, name) in [
        (1, "accept"), (2, "bind"), (3, "closesocket"), (4, "connect"), (5, "getpeername"), (6, "getsockname"),
        (7, "getsockopt"), (8, "htonl"), (9, "htons"), (10, "ioctlsocket"), (11, "inet_addr"), (12, "inet_ntoa"),
        (13, "listen"), (14, "ntohl"), (15, "ntohs"), (16, "recv"), (17, "recvfrom"), (18, "select"), (19, "send"),
        (20, "sendto"), (21, "setsockopt"), (22, "shutdown"), (23, "socket"), (51, "gethostbyaddr"),
        (52, "gethostbyname"), (53, "getprotobyname"), (55, "getservbyname"), (57, "gethostname"),
        (101, "WSAAsyncSelect"), (111, "WSAGetLastError"), (112, "WSASetLastError"), (115, "WSAStartup"),
        (116, "WSACleanup"), (151, "__WSAFDIsSet"),
    ] {
        r.alias("ws2_32.dll", &format!("#{ord}"), "ws2_32.dll", name);
        r.alias("wsock32.dll", &format!("#{ord}"), "ws2_32.dll", name);
    }
    for name in ["WSAStartup", "WSACleanup", "WSAGetLastError", "socket", "closesocket", "connect", "bind", "listen", "accept", "send", "sendto", "recv", "recvfrom", "select", "ioctlsocket", "setsockopt", "getsockopt", "getsockname", "getpeername", "shutdown", "gethostname", "gethostbyname", "gethostbyaddr", "inet_addr", "inet_ntoa", "htons", "ntohs", "htonl", "ntohl", "WSAAsyncSelect", "__WSAFDIsSet"] {
        r.alias("wsock32.dll", name, "ws2_32.dll", name);
    }
    // Wine's wined3d (supplied for its HLSL compiler) imports the kernel-mode display
    // thunks and WGL; there is no OpenGL or display driver behind them.
    const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
    for name in [
        "D3DKMTCloseAdapter", "D3DKMTCreateDCFromMemory", "D3DKMTCreateDevice", "D3DKMTDestroyDCFromMemory",
        "D3DKMTDestroyDevice", "D3DKMTEscape", "D3DKMTOpenAdapterFromGdiDisplayName", "D3DKMTOpenAdapterFromLuid",
        "D3DKMTQueryVideoMemoryInfo", "D3DKMTSetVidPnSourceOwner", "D3DKMTOpenAdapterFromHdc", "D3DKMTQueryStatistics",
    ] {
        r.add("gdi32.dll", name, |_c| ok(1, STATUS_NOT_SUPPORTED));
    }
    const_apis!(r, "opengl32.dll", {
        "glEnable" => (1, 0),
        "glDisable" => (1, 0),
        "wglCreateContext" => (1, 0),
        "wglDeleteContext" => (1, 0),
        "wglGetCurrentContext" => (0, 0),
        "wglGetCurrentDC" => (0, 0),
        "wglGetProcAddress" => (1, 0),
        "wglMakeCurrent" => (2, 0),
        "wglShareLists" => (2, 0),
    });
    const_apis!(r, "wininet.dll", {
        "InternetOpenA" => (5, 0),
        "InternetOpenW" => (5, 0),
        "InternetConnectA" => (8, 0),
        "InternetConnectW" => (8, 0),
        "InternetCloseHandle" => (1, 1),
        "InternetReadFile" => (4, 0),
        "InternetWriteFile" => (4, 0),
        "InternetOpenUrlA" => (6, 0),
        "InternetOpenUrlW" => (6, 0),
        "InternetGetConnectedState" => (2, 0),
        "InternetCanonicalizeUrlA" => (4, 0),
        "InternetCrackUrlA" => (4, 0),
        "InternetCrackUrlW" => (4, 0),
        "InternetSetOptionA" => (4, 0),
        "InternetQueryOptionA" => (4, 0),
        "InternetQueryDataAvailable" => (4, 0),
        "InternetAttemptConnect" => (1, 1225),
        "InternetCheckConnectionA" => (3, 0),
        "InternetGetLastResponseInfoA" => (3, 0),
        "HttpOpenRequestA" => (8, 0),
        "HttpOpenRequestW" => (8, 0),
        "HttpSendRequestA" => (5, 0),
        "HttpSendRequestW" => (5, 0),
        "HttpSendRequestExA" => (5, 0),
        "HttpEndRequestA" => (4, 0),
        "HttpEndRequestW" => (4, 0),
        "HttpQueryInfoA" => (5, 0),
        "HttpQueryInfoW" => (5, 0),
        "HttpAddRequestHeadersA" => (4, 0),
        "FtpOpenFileA" => (5, 0),
        "FtpGetFileA" => (7, 0),
        "FtpPutFileA" => (5, 0),
        "DeleteUrlCacheEntryA" => (1, 0),
        "DeleteUrlCacheEntry" => (1, 0),
    });
    // As on a computer without a connection: a session opens, every connection fails
    // with ERROR_INTERNET_NAME_NOT_RESOLVED. A runner that cannot open a session issues
    // no request and never reports one as failed (GameMaker Studio's http_get: a game
    // waiting for the answer waits forever).
    const ERROR_INTERNET_NAME_NOT_RESOLVED: u32 = 12007;
    for name in ["InternetOpenA", "InternetOpenW"] {
        r.add_n("wininet.dll", name, 5, |c| {
            let h = c.p.new_handle(Handle::Dummy("internet session"));
            ok(5, h)
        });
    }
    for (name, n) in [("InternetConnectA", 8), ("InternetConnectW", 8), ("InternetOpenUrlA", 6), ("InternetOpenUrlW", 6), ("HttpOpenRequestA", 8), ("HttpOpenRequestW", 8), ("HttpSendRequestA", 5), ("HttpSendRequestW", 5), ("HttpSendRequestExA", 5)] {
        r.add_n("wininet.dll", name, n, |c| {
            c.set_last_error(ERROR_INTERNET_NAME_NOT_RESOLVED);
            ok(c.nargs(), 0)
        });
    }
    // InternetCanonicalizeUrlA(url, buffer, pLength, flags): the URL as it is.
    r.add_n("wininet.dll", "InternetCanonicalizeUrlA", 4, |c| {
        let (url, buf, plen) = (c.str_arg(0), c.arg(1), c.arg(2));
        let cap = c.host.read_u32(plen).unwrap_or(0);
        if (url.len() as u32) >= cap || buf == 0 {
            let _ = c.host.write_u32(plen, url.len() as u32 + 1);
            c.set_last_error(122); // ERROR_INSUFFICIENT_BUFFER
            return ok(4, 0);
        }
        let _ = c.host.write_cstr(buf, url.as_bytes());
        let _ = c.host.write_u32(plen, url.len() as u32);
        ok(4, 1)
    });
    // InternetCrackUrlA(url, length, flags, URL_COMPONENTSA*): scheme, host, port, user,
    // password, path and extra info, into the caller's buffers or as pointers into the URL.
    r.add_n("wininet.dll", "InternetCrackUrlA", 4, |c| {
        let (url_p, len, comps) = (c.arg(0), c.arg(1), c.arg(3));
        let url = if len == 0 { c.host.read_cstr_lossy(url_p) } else { let mut b = vec![0u8; len as usize]; let _ = c.host.read(url_p, &mut b); String::from_utf8_lossy(&b).into_owned() };
        let Some((scheme, rest)) = url.split_once("://") else {
            c.set_last_error(12006); // ERROR_INTERNET_UNRECOGNIZED_SCHEME
            return ok(4, 0);
        };
        let (auth_host, path_all) = rest.find('/').map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
        let (userinfo, hostport) = auth_host.rsplit_once('@').map_or(("", auth_host), |(u, h)| (u, h));
        let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse::<u16>().unwrap()),
            _ => (hostport, if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 }),
        };
        let (path, extra) = path_all.find(['?', '#']).map_or((path_all, ""), |i| (&path_all[..i], &path_all[i..]));
        let offset = |part: &str| part.as_ptr() as usize - url.as_ptr() as usize;
        // URL_COMPONENTSA: dwStructSize, then pointer/length pairs (scheme, host, user,
        // password, path, extra) with nScheme after the scheme's and nPort after the host's,
        // each pointer aligned to its size
        let ps = c.ptr_size();
        let align = |x: u32| x.div_ceil(ps) * ps;
        let (scheme_at, nscheme_at) = (ps, 2 * ps + 4);
        let host_at = nscheme_at + 4;
        let nport_at = host_at + ps + 4;
        let user_at = align(nport_at + 2);
        let pass_at = align(user_at + ps + 4);
        let path_at = align(pass_at + ps + 4);
        let extra_at = align(path_at + ps + 4);
        let pairs = [(scheme_at, scheme), (host_at, host), (user_at, user), (pass_at, pass), (path_at, path), (extra_at, extra)];
        for (at, part) in pairs {
            let (pp, pl) = (comps + at, comps + at + ps);
            let want = c.host.read_u32(pl).unwrap_or(0);
            let dst = c.read_ptr(pp);
            if want == 0 {
                continue;
            }
            if dst != 0 {
                if part.len() as u32 >= want {
                    c.set_last_error(122);
                    return ok(4, 0);
                }
                let _ = c.host.write_cstr(dst, part.as_bytes());
            } else {
                let into = if part.is_empty() { 0 } else { url_p + offset(part) as u32 };
                c.write_ptr(pp, into);
            }
            let _ = c.host.write_u32(pl, part.len() as u32);
        }
        // nScheme: INTERNET_SCHEME_HTTP 3, HTTPS 4
        let n = if scheme.eq_ignore_ascii_case("https") { 4 } else if scheme.eq_ignore_ascii_case("http") { 3 } else { 0 };
        let _ = c.host.write_u32(comps + nscheme_at, n);
        let _ = c.host.write_u16(comps + nport_at, port);
        ok(4, 1)
    });
    r.add_n("wininet.dll", "InternetCloseHandle", 1, |c| {
        c.p.handles.remove(&c.arg(0));
        ok(1, 1)
    });
    const_apis!(r, "urlmon.dll", {
        "URLDownloadToFileA" => (5, E_FAIL),
        "URLDownloadToFileW" => (5, E_FAIL),
        "URLDownloadToCacheFileA" => (6, E_FAIL),
        "CreateURLMoniker" => (3, E_FAIL),
        "ObtainUserAgentString" => (3, E_FAIL),
    });
    const_apis!(r, "mpr.dll", {
        "WNetGetConnectionA" => (3, 2250),
        "WNetGetConnectionW" => (3, 2250),
        "WNetOpenEnumA" => (5, 1222),
        "WNetOpenEnumW" => (5, 1222),
        "WNetEnumResourceA" => (4, 259),
        "WNetCloseEnum" => (1, 0),
        "WNetAddConnection2A" => (4, 1222),
        "WNetCancelConnection2A" => (3, 1222),
        "WNetGetUniversalNameA" => (5, 2250),
        "WNetGetUniversalNameW" => (5, 2250),
    });
    const_apis!(r, "winspool.drv", {
        "EnumPrintersA" => (7, 0),
        "EnumPrintersW" => (7, 0),
        "OpenPrinterA" => (3, 0),
        "OpenPrinterW" => (3, 0),
        "ClosePrinter" => (1, 1),
        "DocumentPropertiesA" => (6, 0xFFFF_FFFF),
        "DocumentPropertiesW" => (6, 0xFFFF_FFFF),
        "GetDefaultPrinterA" => (2, 0),
        "GetDefaultPrinterW" => (2, 0),
        "GetPrinterA" => (5, 0),
        "GetPrinterW" => (5, 0),
        "GetPrinterDriverA" => (6, 0),
        "GetPrinterDriverW" => (6, 0),
        "DeviceCapabilitiesA" => (5, 0xFFFF_FFFF),
        "DeviceCapabilitiesW" => (5, 0xFFFF_FFFF),
        "AdvancedDocumentPropertiesA" => (5, 0),
        "StartDocPrinterA" => (3, 0),
        "EndDocPrinter" => (1, 0),
        "StartPagePrinter" => (1, 0),
        "EndPagePrinter" => (1, 0),
        "WritePrinter" => (4, 0),
        "EnumJobsA" => (8, 0),
        "SetJobA" => (5, 0),
        "GetJobA" => (6, 0),
        "AddPrinterConnectionA" => (1, 0),
    });
    const_apis!(r, "ddraw.dll", {
        "DirectDrawCreate" => (3, E_FAIL),
        "DirectDrawCreateEx" => (4, E_FAIL),
        "DirectDrawEnumerateA" => (2, 0),
        "DirectDrawEnumerateW" => (2, 0),
        "DirectDrawEnumerateExA" => (3, 0),
        "DirectDrawEnumerateExW" => (3, 0),
    });
    // Installable drivers (ACM codecs open theirs this way): there are none.
    const_apis!(r, "winmm.dll", {
        "OpenDriver" => (3, 0),
        "CloseDriver" => (3, 0),
        "SendDriverMessage" => (4, 0),
        "DefDriverProc" => (5, 0),
        "DrvGetModuleHandle" => (1, 0),
        "GetDriverModuleHandle" => (1, 0),
    });
    const_apis!(r, "dinput.dll", {
        "DirectInputCreateA" => (4, E_NOINTERFACE),
        "DirectInputCreateW" => (4, E_NOINTERFACE),
        "DirectInputCreateEx" => (5, E_NOINTERFACE),
    });
    const_apis!(r, "dsound.dll", {
        "DirectSoundCreate" => (3, 0x8878_0078),
        "DirectSoundCreate8" => (3, 0x8878_0078),
        "DirectSoundEnumerateA" => (2, 0),
        "DirectSoundEnumerateW" => (2, 0),
        "DirectSoundCaptureCreate" => (3, 0x8878_0078),
        "DirectSoundCaptureCreate8" => (3, 0x8878_0078),
        "DirectSoundCaptureEnumerateA" => (2, 0),
        "DirectSoundCaptureEnumerateW" => (2, 0),
        "GetDeviceID" => (2, 0x8878_0078),
        "DirectSoundFullDuplexCreate" => (10, 0x8878_0078),
    });
    const_apis!(r, "dplayx.dll", {
        "DirectPlayCreate" => (3, E_FAIL),
        "DirectPlayEnumerateA" => (2, 0),
        "DirectPlayEnumerateW" => (2, 0),
        "DirectPlayLobbyCreateA" => (5, E_FAIL),
        "DirectPlayLobbyCreateW" => (5, E_FAIL),
    });
    const_apis!(r, "xinput1_3.dll", {
        "XInputGetState" => (2, 1167),
        "XInputSetState" => (2, 1167),
        "XInputGetCapabilities" => (3, 1167),
        "XInputEnable" => (1, 0),
        "XInputGetBatteryInformation" => (3, 1167),
        "XInputGetKeystroke" => (3, 1167),
        "XInputGetDSoundAudioDeviceGuids" => (3, 1167),
    });
    for name in ["XInputGetState", "XInputSetState", "XInputGetCapabilities", "XInputEnable", "XInputGetBatteryInformation", "XInputGetKeystroke"] {
        r.alias("xinput1_4.dll", name, "xinput1_3.dll", name);
        r.alias("xinput9_1_0.dll", name, "xinput1_3.dll", name);
    }
    const_apis!(r, "psapi.dll", {
        "GetProcessMemoryInfo" => (3, 0),
        "EnumProcesses" => (3, 0),
        "EnumProcessModules" => (4, 0),
        "GetModuleFileNameExA" => (4, 0),
        "GetModuleBaseNameA" => (4, 0),
        "GetModuleInformation" => (4, 0),
        "GetPerformanceInfo" => (2, 0),
    });
    const_apis!(r, "setupapi.dll", {
        "SetupDiGetClassDevsA" => (4, 0xFFFF_FFFF),
        "SetupDiGetClassDevsW" => (4, 0xFFFF_FFFF),
        "SetupDiEnumDeviceInterfaces" => (5, 0),
        "SetupDiGetDeviceInterfaceDetailA" => (6, 0),
        "SetupDiGetDeviceInterfaceDetailW" => (6, 0),
        "SetupDiDestroyDeviceInfoList" => (1, 1),
        "SetupDiEnumDeviceInfo" => (3, 0),
        "SetupDiGetDeviceRegistryPropertyA" => (7, 0),
    });
    const_apis!(r, "hid.dll", {
        "HidD_GetHidGuid" => (1, 0),
        "HidD_GetAttributes" => (2, 0),
        "HidD_GetPreparsedData" => (2, 0),
        "HidD_FreePreparsedData" => (1, 1),
        "HidP_GetCaps" => (2, 0xC011_0001),
        "HidD_GetProductString" => (3, 0),
    });
    const_apis!(r, "dbghelp.dll", {
        "SymInitialize" => (3, 0),
        "SymCleanup" => (1, 0),
        "SymFromAddr" => (4, 0),
        "SymGetLineFromAddr" => (4, 0),
        "SymGetLineFromAddr64" => (4, 0),
        "SymSetOptions" => (1, 0),
        "SymGetOptions" => (0, 0),
        "SymLoadModule" => (6, 0),
        "SymGetModuleInfo" => (3, 0),
        "SymGetSymFromAddr" => (4, 0),
        "StackWalk" => (9, 0),
        "StackWalk64" => (9, 0),
        "SymFunctionTableAccess" => (2, 0),
        "SymFunctionTableAccess64" => (2, 0),
        "SymGetModuleBase" => (2, 0),
        "SymGetModuleBase64" => (2, 0),
        "MiniDumpWriteDump" => (7, 0),
        "UnDecorateSymbolName" => (4, 0),
    });
    const_apis!(r, "dbghelp.dll", {
        "SymInitializeW" => (3, 0),
        "SymFromAddrW" => (4, 0),
        "SymGetLineFromAddrW64" => (4, 0),
        "SymLoadModuleExW" => (8, 0),
        "SymGetModuleInfoW" => (3, 0),
        "SymGetModuleInfoW64" => (3, 0),
        "SymGetSearchPathW" => (3, 0),
        "SymSetSearchPathW" => (2, 0),
        "SymGetSymFromAddr64" => (4, 0),
        "SymLoadModule64" => (6, 0),
        "SymGetModuleInfo64" => (3, 0),
        "SymUnloadModule" => (2, 0),
        "SymUnloadModule64" => (3, 0),
        "SymEnumerateModules" => (3, 0),
    });
    r.add("dbghelp.dll", "ImageDirectoryEntryToData", |c| {
        let (base, mapped, dir, psize) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let _ = mapped;
        let v = image_directory(c, base, dir, psize);
        ok(4, v)
    });
    r.add("dbghelp.dll", "ImageDirectoryEntryToDataEx", |c| {
        let (base, dir, psize, phdr) = (c.arg(0), c.arg(2), c.arg(3), c.arg(4));
        if phdr != 0 {
            let _ = c.host.write_u32(phdr, 0);
        }
        let v = image_directory(c, base, dir, psize);
        ok(5, v)
    });
    r.alias("imagehlp.dll", "ImageDirectoryEntryToData", "dbghelp.dll", "ImageDirectoryEntryToData");
    r.alias("imagehlp.dll", "ImageDirectoryEntryToDataEx", "dbghelp.dll", "ImageDirectoryEntryToDataEx");
    r.add("dbghelp.dll", "ImageNtHeader", |c| {
        let base = c.arg(0);
        let e = c.host.read_u32(base + 0x3C).unwrap_or(0);
        let ok_ = c.host.read_u16(base).unwrap_or(0) == 0x5A4D && c.host.read_u32(base + e).unwrap_or(0) == 0x4550;
        ok(1, if ok_ { base + e } else { 0 })
    });
    r.alias("imagehlp.dll", "ImageNtHeader", "dbghelp.dll", "ImageNtHeader");
    r.alias("ntdll.dll", "RtlImageNtHeader", "dbghelp.dll", "ImageNtHeader");
    r.add("ntdll.dll", "RtlImageDirectoryEntryToData", |c| {
        let (base, dir, psize) = (c.arg(0), c.arg(2), c.arg(3));
        let v = image_directory(c, base, dir, psize);
        ok(4, v)
    });
    for name in ["SymInitializeW", "SymFromAddrW", "SymGetLineFromAddrW64", "SymLoadModuleExW", "SymGetModuleInfoW", "SymGetModuleInfoW64", "SymGetSearchPathW", "SymSetSearchPathW", "SymGetSymFromAddr64", "SymLoadModule64", "SymGetModuleInfo64", "SymUnloadModule", "SymUnloadModule64", "SymEnumerateModules", "SymGetLineFromAddr64", "StackWalk64", "SymFunctionTableAccess64", "SymGetModuleBase64"] {
        r.alias("imagehlp.dll", name, "dbghelp.dll", name);
    }
    for name in ["SymInitialize", "SymCleanup", "SymGetLineFromAddr", "StackWalk", "SymFunctionTableAccess", "SymGetModuleBase", "SymGetSymFromAddr", "UnDecorateSymbolName", "SymSetOptions", "SymGetOptions", "SymLoadModule", "SymGetModuleInfo"] {
        r.alias("imagehlp.dll", name, "dbghelp.dll", name);
    }
    const_apis!(r, "usp10.dll", {
        "ScriptStringAnalyse" => (14, E_NOTIMPL),
        "ScriptStringFree" => (1, S_OK),
        "ScriptStringOut" => (8, E_NOTIMPL),
        "ScriptStringCPtoX" => (4, E_NOTIMPL),
        "ScriptStringXtoCP" => (4, E_NOTIMPL),
        "ScriptItemize" => (7, E_NOTIMPL),
        "ScriptFreeCache" => (1, S_OK),
        "ScriptIsComplex" => (3, S_FALSE),
        "ScriptGetProperties" => (2, E_NOTIMPL),
        "ScriptBreak" => (4, E_NOTIMPL),
        "ScriptShape" => (10, E_NOTIMPL),
        "ScriptPlace" => (9, E_NOTIMPL),
        "ScriptTextOut" => (15, E_NOTIMPL),
    });
    const_apis!(r, "shlwapi.dll", {
        "PathFileExistsA" => (1, 0),
        "PathFileExistsW" => (1, 0),
        "PathIsDirectoryA" => (1, 0),
        "PathIsRelativeA" => (1, 0),
        "PathRemoveFileSpecA" => (1, 1),
        "PathRemoveFileSpecW" => (1, 1),
        "PathStripPathA" => (1, 0),
        "PathStripPathW" => (1, 0),
        "PathFindFileNameA" => (1, 0),
        "PathFindExtensionA" => (1, 0),
        "PathAppendA" => (2, 1),
        "PathCombineA" => (3, 0),
        "PathIsUNCA" => (1, 0),
        "PathIsUNCW" => (1, 0),
        "PathMatchSpecA" => (2, 0),
        "PathAddBackslashA" => (1, 0),
        "StrCmpNIA" => (3, 0),
        "StrCmpNIW" => (3, 0),
        "StrStrIA" => (2, 0),
        "StrStrIW" => (2, 0),
        "StrChrA" => (2, 0),
        "StrChrW" => (2, 0),
        "StrRChrA" => (3, 0),
        "StrRChrW" => (3, 0),
        "StrToIntA" => (1, 0),
        "StrToIntW" => (1, 0),
        "StrDupA" => (1, 0),
        "StrDupW" => (1, 0),
        "SHDeleteKeyA" => (2, 0),
        "SHDeleteKeyW" => (2, 0),
        "SHGetValueA" => (6, 2),
        "SHGetValueW" => (6, 2),
        "SHSetValueA" => (6, 0),
        "SHSetValueW" => (6, 0),
        "SHRegGetPathA" => (5, 2),
        "SHCreateStreamOnFileA" => (3, E_FAIL),
        "SHCreateStreamOnFileW" => (3, E_FAIL),
        "SHCreateStreamOnFileEx" => (6, E_FAIL),
        "SHCreateMemStream" => (2, 0),
        "StrFormatByteSizeA" => (3, 0),
        "wnsprintfA" => (4, 0),
        "UrlEscapeA" => (4, E_FAIL),
        "UrlUnescapeA" => (4, E_FAIL),
        "PathQuoteSpacesA" => (1, 0),
        "PathUnquoteSpacesA" => (1, 0),
        "IsOS" => (1, 0),
        "AssocQueryStringA" => (6, E_FAIL),
        "SHAutoComplete" => (2, E_FAIL),
    });
}
