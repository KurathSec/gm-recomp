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

pub fn register(r: &mut Registry) {
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
        "CoCreateInstance" => (5, REGDB_E_CLASSNOTREG),
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
    r.add(v, "GetFileVersionInfoSizeA", |c| {
        let h = c.arg(1);
        if h != 0 {
            let _ = c.host.write_u32(h, 0);
        }
        c.set_last_error(1813);
        ok(2, 0)
    });
    r.alias(v, "GetFileVersionInfoSizeW", v, "GetFileVersionInfoSizeA");
    const_apis!(r, v, {
        "GetFileVersionInfoA" => (4, 0),
        "GetFileVersionInfoW" => (4, 0),
        "VerQueryValueA" => (4, 0),
        "VerQueryValueW" => (4, 0),
        "VerLanguageNameA" => (3, 0),
    });

    // ---- winmm -----------------------------------------------------------------------
    let w = "winmm.dll";
    r.add(w, "timeGetTime", |c| ok(0, c.p.now_ms()));
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
        "mciSendCommandA" => (4, 262),
        "mciSendCommandW" => (4, 262),
        "mciSendStringA" => (4, 262),
        "mciSendStringW" => (4, 262),
        "mciGetErrorStringA" => (3, 0),
        "mciGetErrorStringW" => (3, 0),
        "mciGetDeviceIDA" => (1, 0),
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
        let h = c.p.new_handle(Handle::Dummy("imagelist"));
        ok(5, h)
    });
    r.add(cc, "ImageList_Destroy", |c| {
        let h = c.arg(0);
        c.p.handles.remove(&h);
        ok(1, 1)
    });
    r.add(cc, "ImageList_GetIconSize", |c| {
        let cx = c.arg(1);
        let cy = c.arg(2);
        let _ = c.host.write_u32(cx, 16);
        let _ = c.host.write_u32(cy, 16);
        ok(3, 1)
    });
    const_apis!(r, cc, {
        "ImageList_Add" => (3, 0),
        "ImageList_AddMasked" => (3, 0),
        "ImageList_AddIcon" => (2, 0),
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
        "ImageList_GetBkColor" => (1, 0xFFFF_FFFF),
        "ImageList_SetBkColor" => (2, 0xFFFF_FFFF),
        "ImageList_GetDragImage" => (2, 0),
        "ImageList_GetIcon" => (3, 0),
        "ImageList_GetImageCount" => (1, 0),
        "ImageList_GetImageInfo" => (3, 0),
        "ImageList_LoadImageA" => (7, 0),
        "ImageList_LoadImageW" => (7, 0),
        "ImageList_Read" => (1, 0),
        "ImageList_Write" => (2, 0),
        "ImageList_Remove" => (2, 1),
        "ImageList_Replace" => (4, 1),
        "ImageList_ReplaceIcon" => (3, 0),
        "ImageList_SetIconSize" => (3, 1),
        "ImageList_SetImageCount" => (2, 1),
        "ImageList_SetOverlayImage" => (3, 1),
        "ImageList_Merge" => (6, 0),
        "ImageList_Duplicate" => (1, 0),
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
        let _ = c.host.write_u32(token, 1);
        if output != 0 {
            let _ = c.host.write_u64(output, 0);
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
        "htons" => (1, 0),
        "ntohs" => (1, 0),
        "htonl" => (1, 0),
        "ntohl" => (1, 0),
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
    for name in ["WSAStartup", "WSACleanup", "WSAGetLastError", "socket", "closesocket", "connect", "bind", "listen", "accept", "send", "sendto", "recv", "recvfrom", "select", "ioctlsocket", "setsockopt", "getsockopt", "getsockname", "getpeername", "shutdown", "gethostname", "gethostbyname", "gethostbyaddr", "inet_addr", "inet_ntoa", "htons", "ntohs", "htonl", "ntohl", "WSAAsyncSelect", "__WSAFDIsSet"] {
        r.alias("wsock32.dll", name, "ws2_32.dll", name);
    }
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
    const_apis!(r, "dinput8.dll", {
        "DirectInput8Create" => (5, E_NOINTERFACE),
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
        "ImageNtHeader" => (1, 0),
        "UnDecorateSymbolName" => (4, 0),
    });
    for name in ["SymInitialize", "SymCleanup", "SymGetLineFromAddr", "StackWalk", "SymFunctionTableAccess", "SymGetModuleBase", "SymGetSymFromAddr", "ImageNtHeader", "UnDecorateSymbolName", "SymSetOptions", "SymGetOptions", "SymLoadModule", "SymGetModuleInfo"] {
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
