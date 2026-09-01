//! kernel32 / ntdll: memory, modules, files, threads (single-threaded for now),
//! synchronization, TLS, strings, time and process information.

use super::{Regs, ApiError, ApiResult, Ctx, Registry, Ret, ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_MOD_NOT_FOUND, ERROR_NO_MORE_FILES, ERROR_PROC_NOT_FOUND, ERROR_SUCCESS, INVALID_HANDLE_VALUE, ERROR_ENVVAR_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER};
use crate::mem::{Guest, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, MEM_TOP_DOWN, PAGE_READWRITE};
use crate::process::{FileHandle, FindEntry, Handle, Process, RET_TO_HOST, TEB_BASE};

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

fn write_out(c: &mut Ctx, addr: u32, v: u32) {
    if addr != 0 {
        let _ = c.host.write_u32(addr, v);
    }
}

/// Writes a byte string into a sized buffer; returns the length the API should report.
pub fn copy_str_out(c: &mut Ctx, buf: u32, cap: u32, s: &[u8]) -> u32 {
    if buf == 0 || cap == 0 {
        return s.len() as u32 + 1;
    }
    let n = s.len().min(cap as usize - 1);
    let _ = c.host.write(buf, &s[..n]);
    let _ = c.host.write_u8(buf + n as u32, 0);
    if n < s.len() { cap } else { n as u32 }
}

pub fn copy_wstr_out(c: &mut Ctx, buf: u32, cap_units: u32, s: &str) -> u32 {
    let units: Vec<u16> = s.encode_utf16().collect();
    if buf == 0 || cap_units == 0 {
        return units.len() as u32 + 1;
    }
    let n = units.len().min(cap_units as usize - 1);
    for (k, u) in units[..n].iter().enumerate() {
        let _ = c.host.write_u16(buf + 2 * k as u32, *u);
    }
    let _ = c.host.write_u16(buf + 2 * n as u32, 0);
    n as u32
}

pub fn register(r: &mut Registry) {
    let k = "kernel32.dll";
    // ---- process / module -------------------------------------------------------
    r.add(k, "GetModuleHandleA", |c| {
        let name = c.str_arg(0);
        let base = module_handle(c, &name);
        c.trace(format!("({name:?}) = {base:#x}"));
        ok(1, base)
    });
    r.add(k, "GetModuleHandleW", |c| {
        let name = c.wstr_arg(0);
        let base = module_handle(c, &name);
        ok(1, base)
    });
    r.add(k, "GetModuleHandleExW", |c| {
        let name = if c.arg(0) & 4 != 0 { String::new() } else { c.wstr_arg(1) };
        let base = module_handle(c, &name);
        write_out(c, c.arg(2), base);
        ok(3, (base != 0) as u32)
    });
    r.add(k, "GetModuleFileNameA", |c| {
        let base = c.arg(0);
        let path = module_path(c, base);
        let n = copy_str_out(c, c.arg(1), c.arg(2), path.as_bytes());
        c.trace(format!("({base:#x}) = {path:?}"));
        ok(3, n)
    });
    r.add(k, "GetModuleFileNameW", |c| {
        let base = c.arg(0);
        let path = module_path(c, base);
        let n = copy_wstr_out(c, c.arg(1), c.arg(2), &path);
        ok(3, n)
    });
    r.add(k, "LoadLibraryA", |c| {
        let name = c.str_arg(0);
        let h = load_library(c, &name)?;
        c.trace(format!("({name:?}) = {h:#x}"));
        ok(1, h)
    });
    r.add(k, "LoadLibraryW", |c| {
        let name = c.wstr_arg(0);
        let h = load_library(c, &name)?;
        c.trace(format!("({name:?}) = {h:#x}"));
        ok(1, h)
    });
    r.add(k, "LoadLibraryExA", |c| {
        let name = c.str_arg(0);
        let h = load_library(c, &name)?;
        c.trace(format!("({name:?}) = {h:#x}"));
        ok(3, h)
    });
    r.add(k, "LoadLibraryExW", |c| {
        let name = c.wstr_arg(0);
        let h = load_library(c, &name)?;
        c.trace(format!("({name:?}) = {h:#x}"));
        ok(3, h)
    });
    r.add(k, "FreeLibrary", |_c| ok(1, 1));
    r.add(k, "GetProcAddress", |c| {
        let base = c.arg(0);
        let name_arg = c.arg(1);
        let name = if name_arg < 0x10000 { format!("#{name_arg}") } else { c.host.read_cstr_lossy(name_arg) };
        let addr = proc_address(c, base, &name);
        c.trace(format!("({base:#x}, {name:?}) = {addr:#x}"));
        if addr == 0 {
            c.set_last_error(ERROR_PROC_NOT_FOUND);
        }
        ok(2, addr)
    });
    r.add(k, "GetCommandLineA", |c| {
        let a = string_const(c, "cmdline_a", &c.p.cmdline.clone());
        ok(0, a)
    });
    r.add(k, "GetCommandLineW", |c| {
        let s = c.p.cmdline.clone();
        let a = wstring_const(c, "cmdline_w", &s);
        ok(0, a)
    });
    r.add(k, "GetVersion", |_c| ok(0, 0x0A28_0105)); // 5.1 build 2600, VER_PLATFORM_WIN32_NT
    r.add(k, "GetVersionExA", |c| {
        let p = c.arg(0);
        let size = c.host.read_u32(p).unwrap_or(0);
        let _ = c.host.write_u32(p + 4, 5);
        let _ = c.host.write_u32(p + 8, 1);
        let _ = c.host.write_u32(p + 12, 2600);
        let _ = c.host.write_u32(p + 16, 2);
        let _ = c.host.write_cstr(p + 20, b"Service Pack 3");
        if size >= 156 {
            let _ = c.host.write_u16(p + 148, 3); // wServicePackMajor
            let _ = c.host.write_u16(p + 152, 0x0100); // wSuiteMask
            let _ = c.host.write_u8(p + 154, 1); // wProductType VER_NT_WORKSTATION
        }
        ok(1, 1)
    });
    r.alias(k, "GetVersionExW", k, "GetVersionExA");
    r.add(k, "GetCurrentProcessId", |_c| ok(0, 0x1234));
    r.add(k, "GetCurrentProcess", |_c| ok(0, 0xFFFF_FFFF));
    r.add(k, "GetCurrentThreadId", |c| ok(0, c.p.main_thread));
    r.add(k, "GetCurrentThread", |_c| ok(0, 0xFFFF_FFFE));
    r.add(k, "ExitProcess", |c| {
        let code = c.arg(0);
        c.p.flush_saves();
        c.p.exit_code = Some(code);
        c.host.request_exit(code);
        ok(1, 0)
    });
    r.add(k, "TerminateProcess", |c| {
        let code = c.arg(1);
        c.p.flush_saves();
        c.p.exit_code = Some(code);
        c.host.request_exit(code);
        ok(2, 1)
    });
    r.add(k, "GetStartupInfoA", |c| {
        let p = c.arg(0);
        let _ = c.host.fill(p, 68, 0);
        let _ = c.host.write_u32(p, 68);
        let _ = c.host.write_u32(p + 44, 1); // dwFlags STARTF_USESHOWWINDOW
        let _ = c.host.write_u16(p + 48, 1); // wShowWindow SW_SHOWNORMAL
        ok(1, 0)
    });
    r.alias(k, "GetStartupInfoW", k, "GetStartupInfoA");
    r.add(k, "GetSystemInfo", |c| {
        let p = c.arg(0);
        let _ = c.host.fill(p, 36, 0);
        let _ = c.host.write_u32(p + 4, 0x1000); // dwPageSize
        let _ = c.host.write_u32(p + 8, 0x10000); // lpMinimumApplicationAddress
        let _ = c.host.write_u32(p + 12, 0x7FFE_FFFF);
        let _ = c.host.write_u32(p + 16, 1); // dwActiveProcessorMask
        let _ = c.host.write_u32(p + 20, 1); // dwNumberOfProcessors
        let _ = c.host.write_u32(p + 24, 586); // dwProcessorType
        let _ = c.host.write_u32(p + 28, 0x10000); // dwAllocationGranularity
        let _ = c.host.write_u16(p + 32, 6); // wProcessorLevel
        ok(1, 0)
    });
    r.alias(k, "GetNativeSystemInfo", k, "GetSystemInfo");
    r.add(k, "SetErrorMode", |_c| ok(1, 0));
    r.add(k, "GetErrorMode", |_c| ok(0, 0));
    r.add(k, "IsDebuggerPresent", |_c| ok(0, 0));
    r.add(k, "IsProcessorFeaturePresent", |c| {
        // PF_XMMI_INSTRUCTIONS_AVAILABLE(6), PF_XMMI64(10), PF_MMX(3) yes; others no
        let f = c.arg(0);
        ok(1, matches!(f, 3 | 6 | 10 | 0) as u32)
    });
    r.add(k, "GetProcessAffinityMask", |c| {
        write_out(c, c.arg(1), 1);
        write_out(c, c.arg(2), 1);
        ok(3, 1)
    });
    r.add(k, "SetProcessAffinityMask", |_c| ok(2, 1));
    r.add(k, "SetThreadAffinityMask", |_c| ok(2, 1));
    r.add(k, "SetPriorityClass", |_c| ok(2, 1));
    r.add(k, "GetPriorityClass", |_c| ok(1, 0x20));
    r.add(k, "SetThreadPriority", |_c| ok(2, 1));
    r.add(k, "GetThreadPriority", |_c| ok(1, 0));
    r.add(k, "GetProcessVersion", |_c| ok(1, 0x0005_0001));
    r.add(k, "SetUnhandledExceptionFilter", |_c| ok(1, 0));
    r.add(k, "UnhandledExceptionFilter", |_c| ok(1, 1));
    r.add(k, "GetProcessHeap", |c| ok(0, c.p.process_heap));
    r.add(k, "GetLastError", |c| {
        let e = c.host.read_u32(c.regs.fs_base + 0x34).unwrap_or(0);
        ok(0, e)
    });
    r.add(k, "SetLastError", |c| {
        let e = c.arg(0);
        c.set_last_error(e);
        ok(1, 0)
    });
    r.add(k, "RaiseException", |c| {
        let code = c.arg(0);
        let flags = c.arg(1);
        let n = c.arg(2).min(15);
        let args_p = c.arg(3);
        let params: Vec<u32> = (0..n).map(|i| c.host.read_u32(args_p + 4 * i).unwrap_or(0)).collect();
        let ra = c.host.read_u32(c.regs.esp).unwrap_or(0);
        c.trace(format!("({code:#x}, {flags:#x}, {params:x?}) from {ra:#x}"));
        // The context describes the caller as if RaiseException had returned.
        let mut regs = *c.regs;
        regs.eip = ra;
        regs.esp += 4 + 16;
        dispatch_exception(c, code, flags, ra, &params, &regs)
    });
    r.add(k, "RtlUnwind", |c| {
        let target = c.arg(0);
        let rec = c.arg(2);
        let retval = c.arg(3);
        c.trace(format!("(frame {target:#x}, rec {rec:#x})"));
        unwind_frames(c, target, rec)?;
        ok(4, retval)
    });
    r.add(k, "OutputDebugStringA", |c| {
        let s = c.str_arg(0);
        c.p.log(format!("OutputDebugString: {s}"));
        ok(1, 0)
    });
    r.add(k, "OutputDebugStringW", |c| {
        let s = c.wstr_arg(0);
        c.p.log(format!("OutputDebugString: {s}"));
        ok(1, 0)
    });
    r.add(k, "DebugBreak", |_c| ok(0, 0));
    r.add(k, "GetEnvironmentStrings", |c| {
        let mut blob = Vec::new();
        for (kname, v) in &c.p.env {
            blob.extend_from_slice(format!("{kname}={v}").as_bytes());
            blob.push(0);
        }
        blob.push(0);
        let a = c.p.heap_alloc(c.host, 0, blob.len() as u32, false);
        let _ = c.host.write(a, &blob);
        ok(0, a)
    });
    r.alias(k, "GetEnvironmentStringsA", k, "GetEnvironmentStrings");
    r.add(k, "GetEnvironmentStringsW", |c| {
        let mut units: Vec<u16> = Vec::new();
        for (kname, v) in &c.p.env {
            units.extend(format!("{kname}={v}").encode_utf16());
            units.push(0);
        }
        units.push(0);
        let a = c.p.heap_alloc(c.host, 0, units.len() as u32 * 2, false);
        for (i, u) in units.iter().enumerate() {
            let _ = c.host.write_u16(a + 2 * i as u32, *u);
        }
        ok(0, a)
    });
    r.add(k, "FreeEnvironmentStringsA", |_c| ok(1, 1));
    r.add(k, "FreeEnvironmentStringsW", |_c| ok(1, 1));
    r.add(k, "GetEnvironmentVariableA", |c| {
        let name = c.str_arg(0);
        match c.p.env.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone()) {
            Some(v) => {
                let n = copy_str_out(c, c.arg(1), c.arg(2), v.as_bytes());
                ok(3, n)
            }
            None => {
                c.set_last_error(ERROR_ENVVAR_NOT_FOUND);
                ok(3, 0)
            }
        }
    });
    r.add(k, "GetEnvironmentVariableW", |c| {
        let name = c.wstr_arg(0);
        match c.p.env.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone()) {
            Some(v) => {
                let n = copy_wstr_out(c, c.arg(1), c.arg(2), &v);
                ok(3, n)
            }
            None => {
                c.set_last_error(ERROR_ENVVAR_NOT_FOUND);
                ok(3, 0)
            }
        }
    });
    r.add(k, "SetEnvironmentVariableA", |c| {
        let (n, v) = (c.str_arg(0), c.str_arg(1));
        c.p.env.retain(|(k2, _)| !k2.eq_ignore_ascii_case(&n));
        c.p.env.push((n, v));
        ok(2, 1)
    });
    r.add(k, "ExpandEnvironmentStringsA", |c| {
        let s = c.str_arg(0);
        let n = copy_str_out(c, c.arg(1), c.arg(2), s.as_bytes());
        ok(3, n)
    });
    r.add(k, "GetCurrentDirectoryA", |c| {
        let cwd = c.p.cwd.clone();
        let n = copy_str_out(c, c.arg(1), c.arg(0), cwd.as_bytes());
        ok(2, n)
    });
    r.add(k, "GetCurrentDirectoryW", |c| {
        let cwd = c.p.cwd.clone();
        let n = copy_wstr_out(c, c.arg(1), c.arg(0), &cwd);
        ok(2, n)
    });
    r.add(k, "SetCurrentDirectoryA", |c| {
        let d = c.str_arg(0);
        c.p.cwd = d;
        ok(1, 1)
    });
    r.add(k, "GetSystemDirectoryA", |c| {
        let n = copy_str_out(c, c.arg(0), c.arg(1), b"C:\\Windows\\system32");
        ok(2, n)
    });
    r.add(k, "GetWindowsDirectoryA", |c| {
        let n = copy_str_out(c, c.arg(0), c.arg(1), b"C:\\Windows");
        ok(2, n)
    });
    r.add(k, "GetTempPathA", |c| {
        let n = copy_str_out(c, c.arg(1), c.arg(0), b"C:\\Temp\\");
        ok(2, n)
    });
    r.add(k, "GetComputerNameA", |c| {
        let n = copy_str_out(c, c.arg(0), c.host.read_u32(c.arg(1)).unwrap_or(0), b"GMRECOMP");
        write_out(c, c.arg(1), n);
        ok(2, 1)
    });
    r.add(k, "GetUserDefaultLCID", |_c| ok(0, 0x0409));
    r.add(k, "GetUserDefaultLangID", |_c| ok(0, 0x0409));
    r.add(k, "GetSystemDefaultLCID", |_c| ok(0, 0x0409));
    r.add(k, "GetSystemDefaultLangID", |_c| ok(0, 0x0409));
    r.add(k, "GetThreadLocale", |_c| ok(0, 0x0409));
    r.add(k, "SetThreadLocale", |_c| ok(1, 1));
    r.add(k, "GetUserDefaultUILanguage", |_c| ok(0, 0x0409));
    r.add(k, "GetSystemDefaultUILanguage", |_c| ok(0, 0x0409));
    r.add(k, "GetACP", |_c| ok(0, 1252));
    r.add(k, "GetOEMCP", |_c| ok(0, 437));
    r.add(k, "GetCPInfo", |c| {
        let p = c.arg(1);
        let _ = c.host.fill(p, 20, 0);
        let _ = c.host.write_u32(p, 1); // MaxCharSize
        let _ = c.host.write_u8(p + 4, b'?');
        ok(2, 1)
    });
    r.add(k, "GetCPInfoExA", |c| {
        let p = c.arg(2);
        let _ = c.host.fill(p, 0x114, 0);
        let _ = c.host.write_u32(p, 1);
        let _ = c.host.write_u8(p + 4, b'?');
        let _ = c.host.write_u32(p + 0x10, 1252);
        ok(3, 1)
    });
    r.add(k, "IsValidCodePage", |_c| ok(1, 1));
    r.add(k, "IsValidLocale", |_c| ok(2, 1));
    r.add(k, "GetLocaleInfoA", |c| {
        let lctype = c.arg(1) & 0xFFFF;
        let v = locale_info(lctype);
        let n = copy_str_out(c, c.arg(2), c.arg(3), v.as_bytes());
        ok(4, n)
    });
    r.add(k, "GetLocaleInfoW", |c| {
        let lctype = c.arg(1) & 0xFFFF;
        let v = locale_info(lctype);
        let n = copy_wstr_out(c, c.arg(2), c.arg(3), v);
        ok(4, n)
    });
    r.add(k, "EnumCalendarInfoA", |_c| ok(4, 1));
    r.add(k, "GetDateFormatA", |c| {
        let n = copy_str_out(c, c.arg(4), c.arg(5), b"1/1/2026");
        ok(6, n)
    });
    r.add(k, "GetTimeFormatA", |c| {
        let n = copy_str_out(c, c.arg(4), c.arg(5), b"12:00:00");
        ok(6, n)
    });
    r.add(k, "GetStringTypeA", |c| {
        // CT_CTYPE1 classification of a byte string
        let (src, n, out) = (c.arg(2), c.arg(3) as i32, c.arg(4));
        let s = if n < 0 { c.host.read_cstr(src).unwrap_or_default() } else { let mut b = vec![0u8; n as usize]; let _ = c.host.read(src, &mut b); b };
        for (i, &b) in s.iter().enumerate() {
            let _ = c.host.write_u16(out + 2 * i as u32, ctype1(b));
        }
        ok(5, 1)
    });
    r.add(k, "GetStringTypeW", |c| {
        let (src, n, out) = (c.arg(1), c.arg(2) as i32, c.arg(3));
        let count = if n < 0 { c.host.read_wstr(src).map(|s| s.encode_utf16().count()).unwrap_or(0) } else { n as usize };
        for i in 0..count {
            let u = c.host.read_u16(src + 2 * i as u32).unwrap_or(0);
            let _ = c.host.write_u16(out + 2 * i as u32, if u < 256 { ctype1(u as u8) } else { 0x100 });
        }
        ok(4, 1)
    });
    r.add(k, "CompareStringA", |c| {
        let (a, na, b, nb) = (c.arg(2), c.arg(3) as i32, c.arg(4), c.arg(5) as i32);
        let sa = read_sized(c, a, na);
        let sb = read_sized(c, b, nb);
        let ignore_case = c.arg(1) & 1 != 0;
        let (x, y) = if ignore_case { (sa.to_ascii_lowercase(), sb.to_ascii_lowercase()) } else { (sa, sb) };
        ok(6, match x.cmp(&y) { std::cmp::Ordering::Less => 1, std::cmp::Ordering::Equal => 2, _ => 3 })
    });
    r.add(k, "CompareStringW", |c| {
        let (a, na, b, nb) = (c.arg(2), c.arg(3) as i32, c.arg(4), c.arg(5) as i32);
        let sa = read_wsized(c, a, na);
        let sb = read_wsized(c, b, nb);
        let ignore_case = c.arg(1) & 1 != 0;
        let (x, y) = if ignore_case { (super::nls::lower_str(&sa), super::nls::lower_str(&sb)) } else { (sa, sb) };
        ok(6, match x.cmp(&y) { std::cmp::Ordering::Less => 1, std::cmp::Ordering::Equal => 2, _ => 3 })
    });
    r.add(k, "LCMapStringA", |c| {
        let flags = c.arg(1);
        let s = read_sized(c, c.arg(2), c.arg(3) as i32);
        let out: Vec<u8> = if flags & 0x100 != 0 { s.to_ascii_lowercase() } else if flags & 0x200 != 0 { s.to_ascii_uppercase() } else { s.clone() };
        let (dst, cap) = (c.arg(4), c.arg(5));
        if cap == 0 {
            return ok(6, out.len() as u32);
        }
        let n = out.len().min(cap as usize);
        let _ = c.host.write(dst, &out[..n]);
        ok(6, n as u32)
    });
    r.add(k, "LCMapStringW", |c| {
        let flags = c.arg(1);
        let s = read_wsized(c, c.arg(2), c.arg(3) as i32);
        let out = if flags & 0x100 != 0 { super::nls::lower_str(&s) } else if flags & 0x200 != 0 { super::nls::upper_str(&s) } else { s };
        let units: Vec<u16> = out.encode_utf16().collect();
        let (dst, cap) = (c.arg(4), c.arg(5));
        if cap == 0 {
            return ok(6, units.len() as u32);
        }
        let n = units.len().min(cap as usize);
        for (i, u) in units[..n].iter().enumerate() {
            let _ = c.host.write_u16(dst + 2 * i as u32, *u);
        }
        ok(6, n as u32)
    });
    r.add(k, "MultiByteToWideChar", |c| {
        let (src, n, dst, cap) = (c.arg(2), c.arg(3) as i32, c.arg(4), c.arg(5));
        let bytes = read_sized(c, src, n);
        let s = decode_cp(c.arg(0), &bytes);
        let mut units: Vec<u16> = s.encode_utf16().collect();
        if n < 0 {
            units.push(0);
        }
        if cap == 0 {
            return ok(6, units.len() as u32);
        }
        let m = units.len().min(cap as usize);
        for (i, u) in units[..m].iter().enumerate() {
            let _ = c.host.write_u16(dst + 2 * i as u32, *u);
        }
        ok(6, m as u32)
    });
    r.add(k, "WideCharToMultiByte", |c| {
        let (src, n, dst, cap) = (c.arg(2), c.arg(3) as i32, c.arg(4), c.arg(5));
        let s = read_wsized(c, src, n);
        let mut bytes = encode_cp(c.arg(0), &s);
        if n < 0 {
            bytes.push(0);
        }
        if cap == 0 {
            return ok(8, bytes.len() as u32);
        }
        let m = bytes.len().min(cap as usize);
        let _ = c.host.write(dst, &bytes[..m]);
        ok(8, m as u32)
    });
    r.add(k, "lstrlenA", |c| {
        let s = c.str_arg(0);
        ok(1, s.len() as u32)
    });
    r.add(k, "lstrlenW", |c| {
        let s = c.wstr_arg(0);
        ok(1, s.encode_utf16().count() as u32)
    });
    r.add(k, "lstrcpyA", |c| {
        let (d, s) = (c.arg(0), c.host.read_cstr(c.arg(1)).unwrap_or_default());
        let _ = c.host.write_cstr(d, &s);
        ok(2, d)
    });
    r.add(k, "lstrcpynA", |c| {
        let (d, s, n) = (c.arg(0), c.host.read_cstr(c.arg(1)).unwrap_or_default(), c.arg(2));
        copy_str_out(c, d, n, &s);
        ok(3, d)
    });
    r.add(k, "lstrcatA", |c| {
        let d = c.arg(0);
        let mut s = c.host.read_cstr(d).unwrap_or_default();
        s.extend(c.host.read_cstr(c.arg(1)).unwrap_or_default());
        let _ = c.host.write_cstr(d, &s);
        ok(2, d)
    });
    r.add(k, "lstrcmpA", |c| {
        let (a, b) = (c.str_arg(0), c.str_arg(1));
        ok(2, match a.cmp(&b) { std::cmp::Ordering::Less => -1i32 as u32, std::cmp::Ordering::Equal => 0, _ => 1 })
    });
    r.add(k, "lstrcmpiA", |c| {
        let (a, b) = (super::nls::lower_str(&c.str_arg(0)), super::nls::lower_str(&c.str_arg(1)));
        ok(2, match a.cmp(&b) { std::cmp::Ordering::Less => -1i32 as u32, std::cmp::Ordering::Equal => 0, _ => 1 })
    });
    r.alias(k, "lstrcmpiW", k, "lstrcmpiA");
    r.alias(k, "lstrcmpW", k, "lstrcmpA");
    // ---- memory ----------------------------------------------------------------
    r.add(k, "VirtualAlloc", |c| {
        let (addr, size, ty, protect) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let r = virtual_alloc(c, addr, size, ty, protect);
        c.trace(format!("({addr:#x}, {size:#x}, {ty:#x}, {protect:#x}) = {r:#x}"));
        ok(4, r)
    });
    r.add(k, "VirtualAllocEx", |c| {
        let (addr, size, ty, protect) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4));
        let r = virtual_alloc(c, addr, size, ty, protect);
        ok(5, r)
    });
    r.add(k, "VirtualFree", |c| {
        let (addr, size, ty) = (c.arg(0), c.arg(1), c.arg(2));
        let r = if ty & MEM_RELEASE != 0 { c.p.vm.release(addr) } else if ty & MEM_DECOMMIT != 0 { c.p.vm.decommit(addr, size) } else { false };
        c.trace(format!("({addr:#x}, {size:#x}, {ty:#x}) = {}", r as u32));
        ok(3, r as u32)
    });
    r.add(k, "VirtualProtect", |c| {
        let (addr, size, protect, old) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        match c.p.vm.protect(addr, size, protect) {
            Some(o) => {
                write_out(c, old, o);
                ok(4, 1)
            }
            None => ok(4, 0),
        }
    });
    r.add(k, "VirtualQuery", |c| {
        let (addr, buf, len) = (c.arg(0), c.arg(1), c.arg(2));
        let (base, size, state, protect, alloc_base) = c.p.vm.query(addr);
        if len >= 28 {
            let _ = c.host.write_u32(buf, base);
            let _ = c.host.write_u32(buf + 4, alloc_base);
            let _ = c.host.write_u32(buf + 8, protect);
            let _ = c.host.write_u32(buf + 12, size);
            let _ = c.host.write_u32(buf + 16, state);
            let _ = c.host.write_u32(buf + 20, protect);
            let _ = c.host.write_u32(buf + 24, if state == crate::mem::MEM_FREE { 0 } else { crate::mem::MEM_PRIVATE });
        }
        ok(3, 28)
    });
    r.add(k, "VirtualLock", |_c| ok(2, 1));
    r.add(k, "VirtualUnlock", |_c| ok(2, 1));
    r.add(k, "HeapCreate", |c| {
        let size = c.arg(2).max(0x0010_0000);
        let h = c.p.create_heap(c.host, size).unwrap_or(0);
        ok(3, h)
    });
    r.add(k, "HeapDestroy", |_c| ok(1, 1));
    r.add(k, "HeapAlloc", |c| {
        let (heap, flags, size) = (c.arg(0), c.arg(1), c.arg(2));
        let a = c.p.heap_alloc(c.host, heap, size, flags & 8 != 0);
        ok(3, a)
    });
    r.add(k, "HeapFree", |c| {
        let a = c.arg(2);
        c.p.heap_free(a);
        ok(3, 1)
    });
    r.add(k, "HeapReAlloc", |c| {
        let (heap, flags, old, size) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let a = heap_realloc(c, heap, old, size, flags & 8 != 0);
        ok(4, a)
    });
    r.add(k, "HeapSize", |c| {
        let a = c.arg(2);
        ok(3, c.p.heap_size(a).unwrap_or(0xFFFF_FFFF))
    });
    r.add(k, "HeapValidate", |_c| ok(3, 1));
    r.add(k, "HeapSetInformation", |_c| ok(4, 1));
    r.add(k, "HeapWalk", |_c| ok(2, 0));
    r.add(k, "HeapCompact", |_c| ok(2, 0));
    r.add(k, "HeapLock", |_c| ok(1, 1));
    r.add(k, "HeapUnlock", |_c| ok(1, 1));
    r.add(k, "GlobalAlloc", |c| {
        let (flags, size) = (c.arg(0), c.arg(1));
        let a = c.p.heap_alloc(c.host, 0, size, flags & 0x40 != 0);
        ok(2, a)
    });
    r.add(k, "GlobalFree", |c| {
        let a = c.arg(0);
        c.p.heap_free(a);
        ok(1, 0)
    });
    r.add(k, "GlobalReAlloc", |c| {
        let (old, size, flags) = (c.arg(0), c.arg(1), c.arg(2));
        let a = heap_realloc(c, 0, old, size, flags & 0x40 != 0);
        ok(3, a)
    });
    r.add(k, "GlobalLock", |c| ok(1, c.arg(0)));
    r.add(k, "GlobalUnlock", |_c| ok(1, 0));
    r.add(k, "GlobalHandle", |c| ok(1, c.arg(0)));
    r.add(k, "GlobalSize", |c| {
        let a = c.arg(0);
        ok(1, c.p.heap_size(a).unwrap_or(0))
    });
    r.add(k, "GlobalFlags", |_c| ok(1, 0));
    r.add(k, "GlobalMemoryStatus", |c| {
        let p = c.arg(0);
        let _ = c.host.write_u32(p, 32);
        let _ = c.host.write_u32(p + 4, 10);
        let _ = c.host.write_u32(p + 8, 0x4000_0000);
        let _ = c.host.write_u32(p + 12, 0x3000_0000);
        let _ = c.host.write_u32(p + 16, 0x8000_0000);
        let _ = c.host.write_u32(p + 20, 0x6000_0000);
        let _ = c.host.write_u32(p + 24, 0x7FFF_0000);
        let _ = c.host.write_u32(p + 28, 0x7000_0000);
        ok(1, 0)
    });
    r.add(k, "GlobalMemoryStatusEx", |c| {
        let p = c.arg(0);
        let _ = c.host.write_u32(p + 4, 10);
        let _ = c.host.write_u64(p + 8, 0x4000_0000);
        let _ = c.host.write_u64(p + 16, 0x3000_0000);
        let _ = c.host.write_u64(p + 24, 0x8000_0000);
        let _ = c.host.write_u64(p + 32, 0x6000_0000);
        let _ = c.host.write_u64(p + 40, 0x7FFF_0000);
        let _ = c.host.write_u64(p + 48, 0x7000_0000);
        ok(1, 1)
    });
    r.alias(k, "LocalAlloc", k, "GlobalAlloc");
    r.alias(k, "LocalFree", k, "GlobalFree");
    r.alias(k, "LocalReAlloc", k, "GlobalReAlloc");
    r.alias(k, "LocalLock", k, "GlobalLock");
    r.alias(k, "LocalUnlock", k, "GlobalUnlock");
    r.alias(k, "LocalSize", k, "GlobalSize");
    r.alias(k, "LocalHandle", k, "GlobalHandle");
    r.add(k, "IsBadReadPtr", |_c| ok(2, 0));
    r.add(k, "IsBadWritePtr", |_c| ok(2, 0));
    r.add(k, "IsBadCodePtr", |_c| ok(1, 0));
    r.add(k, "IsBadStringPtrA", |_c| ok(2, 0));
    r.add(k, "RtlMoveMemory", |c| {
        let (d, s, n) = (c.arg(0), c.arg(1), c.arg(2));
        let mut buf = vec![0u8; n as usize];
        let _ = c.host.read(s, &mut buf);
        let _ = c.host.write(d, &buf);
        ok(3, 0)
    });
    r.alias(k, "RtlCopyMemory", k, "RtlMoveMemory");
    r.add(k, "RtlZeroMemory", |c| {
        let (d, n) = (c.arg(0), c.arg(1));
        let _ = c.host.fill(d, n, 0);
        ok(2, 0)
    });
    r.add(k, "RtlFillMemory", |c| {
        let (d, n, v) = (c.arg(0), c.arg(1), c.arg(2));
        let _ = c.host.fill(d, n, v as u8);
        ok(3, 0)
    });
    r.add(k, "FlushInstructionCache", |_c| ok(3, 1));
    // ---- TLS / threads / synchronization ----------------------------------------
    r.add(k, "TlsAlloc", |c| {
        let i = c.p.tls_used.iter().position(|u| !u).unwrap_or(0);
        c.p.tls_used[i] = true;
        let _ = c.host.write_u32(TEB_BASE + 0xE10 + 4 * i as u32, 0);
        ok(0, i as u32)
    });
    r.add(k, "TlsFree", |c| {
        let i = c.arg(0) as usize;
        if i < c.p.tls_used.len() {
            c.p.tls_used[i] = false;
        }
        ok(1, 1)
    });
    r.add(k, "TlsGetValue", |c| {
        let i = c.arg(0);
        let v = c.host.read_u32(c.regs.fs_base + 0xE10 + 4 * i).unwrap_or(0);
        ok(1, v)
    });
    r.add(k, "TlsSetValue", |c| {
        let (i, v) = (c.arg(0), c.arg(1));
        let _ = c.host.write_u32(c.regs.fs_base + 0xE10 + 4 * i, v);
        ok(2, 1)
    });
    r.add(k, "InitializeCriticalSection", |c| {
        let cs = c.arg(0);
        let _ = c.host.fill(cs, 24, 0);
        let _ = c.host.write_u32(cs + 4, 0xFFFF_FFFF);
        ok(1, 0)
    });
    r.add(k, "InitializeCriticalSectionAndSpinCount", |c| {
        let cs = c.arg(0);
        let _ = c.host.fill(cs, 24, 0);
        let _ = c.host.write_u32(cs + 4, 0xFFFF_FFFF);
        ok(2, 1)
    });
    r.add(k, "InitializeCriticalSectionEx", |c| {
        let cs = c.arg(0);
        let _ = c.host.fill(cs, 24, 0);
        ok(3, 1)
    });
    r.add(k, "EnterCriticalSection", |_c| ok(1, 0));
    r.add(k, "LeaveCriticalSection", |_c| ok(1, 0));
    r.add(k, "TryEnterCriticalSection", |_c| ok(1, 1));
    r.add(k, "DeleteCriticalSection", |_c| ok(1, 0));
    r.add(k, "SetCriticalSectionSpinCount", |_c| ok(2, 0));
    r.add(k, "InterlockedIncrement", |c| {
        let a = c.arg(0);
        let v = c.host.read_u32(a).unwrap_or(0).wrapping_add(1);
        let _ = c.host.write_u32(a, v);
        ok(1, v)
    });
    r.add(k, "InterlockedDecrement", |c| {
        let a = c.arg(0);
        let v = c.host.read_u32(a).unwrap_or(0).wrapping_sub(1);
        let _ = c.host.write_u32(a, v);
        ok(1, v)
    });
    r.add(k, "InterlockedExchange", |c| {
        let (a, v) = (c.arg(0), c.arg(1));
        let old = c.host.read_u32(a).unwrap_or(0);
        let _ = c.host.write_u32(a, v);
        ok(2, old)
    });
    r.add(k, "InterlockedExchangeAdd", |c| {
        let (a, v) = (c.arg(0), c.arg(1));
        let old = c.host.read_u32(a).unwrap_or(0);
        let _ = c.host.write_u32(a, old.wrapping_add(v));
        ok(2, old)
    });
    r.add(k, "InterlockedCompareExchange", |c| {
        let (a, v, cmp) = (c.arg(0), c.arg(1), c.arg(2));
        let old = c.host.read_u32(a).unwrap_or(0);
        if old == cmp {
            let _ = c.host.write_u32(a, v);
        }
        ok(3, old)
    });
    r.add(k, "CreateEventA", |c| {
        let (manual, initial) = (c.arg(1) != 0, c.arg(2) != 0);
        let h = c.p.new_handle(Handle::Event { manual, signaled: initial });
        ok(4, h)
    });
    r.alias(k, "CreateEventW", k, "CreateEventA");
    r.add(k, "SetEvent", |c| {
        let h = c.arg(0);
        if let Some(Handle::Event { signaled, .. }) = c.p.handles.get_mut(&h) {
            *signaled = true;
        }
        ok(1, 1)
    });
    r.add(k, "ResetEvent", |c| {
        let h = c.arg(0);
        if let Some(Handle::Event { signaled, .. }) = c.p.handles.get_mut(&h) {
            *signaled = false;
        }
        ok(1, 1)
    });
    r.add(k, "PulseEvent", |_c| ok(1, 1));
    r.add(k, "CreateMutexA", |c| {
        let h = c.p.new_handle(Handle::Mutex);
        ok(3, h)
    });
    r.alias(k, "CreateMutexW", k, "CreateMutexA");
    r.add(k, "ReleaseMutex", |_c| ok(1, 1));
    r.add(k, "CreateSemaphoreA", |c| {
        let h = c.p.new_handle(Handle::Semaphore);
        ok(4, h)
    });
    r.add(k, "ReleaseSemaphore", |_c| ok(3, 1));
    r.add(k, "WaitForSingleObject", |c| {
        let (h, timeout) = (c.arg(0), c.arg(1));
        // Events: wait until signaled; other objects are always signaled (single thread).
        // A waitable timer becomes signaled at its due time.
        if let Some(i) = c.p.timers_due.iter().position(|&(t, _)| t == h) {
            let at = c.p.timers_due[i].1;
            let now = c.p.clock_us;
            if at > now {
                if timeout != 0xFFFF_FFFF && (timeout as u64) * 1000 < at - now {
                    c.advance_clock(timeout as u64 * 1000);
                    return ok(2, 0x102);
                }
                c.advance_clock(at - now);
            }
            c.p.timers_due.remove(i);
            if let Some(Handle::Event { signaled, manual }) = c.p.handles.get_mut(&h) {
                *signaled = *manual;
            }
            return ok(2, 0);
        }
        let signaled = match c.p.handles.get_mut(&h) {
            Some(Handle::Event { signaled, manual }) => {
                let s = *signaled;
                if s && !*manual {
                    *signaled = false;
                }
                s
            }
            Some(Handle::Thread { .. }) => true,
            _ => true,
        };
        if signaled {
            ok(2, 0)
        } else {
            // advance the clock by the timeout and report it
            c.advance_clock((timeout.min(1000) as u64) * 1000);
            ok(2, if timeout == 0xFFFF_FFFF { 0 } else { 0x102 })
        }
    });
    r.add(k, "WaitForSingleObjectEx", |c| {
        let timeout = c.arg(1);
        c.advance_clock((timeout.min(1000) as u64) * 1000);
        ok(3, 0)
    });
    r.add(k, "WaitForMultipleObjects", |_c| ok(4, 0));
    r.add(k, "CreateThread", |c| {
        let (start, param) = (c.arg(2), c.arg(3));
        let id = 0x1300 + c.p.threads.len() as u32 * 4;
        c.p.log(format!("CreateThread(start={start:#x}, param={param:#x}): threads are not run (id {id:#x})"));
        let h = c.p.new_handle(Handle::Thread { id });
        write_out(c, c.arg(5), id);
        ok(6, h)
    });
    r.add(k, "ResumeThread", |_c| ok(1, 0));
    r.add(k, "SuspendThread", |_c| ok(1, 0));
    r.add(k, "TerminateThread", |_c| ok(2, 1));
    r.add(k, "GetExitCodeThread", |c| {
        write_out(c, c.arg(1), 0x103);
        ok(2, 1)
    });
    r.add(k, "GetExitCodeProcess", |c| {
        write_out(c, c.arg(1), 0x103);
        ok(2, 1)
    });
    r.add(k, "SwitchToThread", |_c| ok(0, 0));
    r.add(k, "Sleep", |c| {
        let ms = c.arg(0);
        c.advance_clock(ms as u64 * 1000);
        ok(1, 0)
    });
    r.add(k, "SleepEx", |c| {
        let ms = c.arg(0);
        c.advance_clock(ms as u64 * 1000);
        ok(2, 0)
    });
    r.add(k, "CloseHandle", |c| {
        let h = c.arg(0);
        c.p.handles.remove(&h);
        ok(1, 1)
    });
    r.add(k, "DuplicateHandle", |c| {
        let src = c.arg(1);
        let out = c.arg(3);
        let cloned = c.p.handles.get(&src).cloned();
        let nh = match cloned {
            Some(h) => c.p.new_handle(h),
            None => src,
        };
        write_out(c, out, nh);
        ok(7, 1)
    });
    r.add(k, "GetHandleInformation", |_c| ok(2, 1));
    r.add(k, "SetHandleInformation", |_c| ok(3, 1));
    r.add(k, "SetHandleCount", |c| ok(1, c.arg(0)));
    // ---- time -----------------------------------------------------------------------
    r.add(k, "GetTickCount", |c| {
        c.poll_clock();
        ok(0, c.p.now_ms())
    });
    r.add(k, "GetTickCount64", |c| {
        c.poll_clock();
        let ms = c.p.clock_us / 1000;
        c.regs.edx = (ms >> 32) as u32;
        ok(0, ms as u32)
    });
    r.add(k, "QueryPerformanceFrequency", |c| {
        let p = c.arg(0);
        let _ = c.host.write_u64(p, 10_000_000);
        ok(1, 1)
    });
    r.add(k, "QueryPerformanceCounter", |c| {
        c.poll_clock();
        let p = c.arg(0);
        let _ = c.host.write_u64(p, c.p.clock_us * 10);
        ok(1, 1)
    });
    r.add(k, "GetSystemTime", |c| {
        write_systemtime(c, c.arg(0));
        ok(1, 0)
    });
    r.alias(k, "GetLocalTime", k, "GetSystemTime");
    r.add(k, "GetSystemTimeAsFileTime", |c| {
        c.poll_clock();
        let p = c.arg(0);
        // 2026-01-01 as FILETIME + virtual clock
        let base: u64 = 0x01DC_5A6B_0000_0000 + c.p.clock_us * 10;
        let _ = c.host.write_u64(p, base);
        ok(1, 0)
    });
    r.add(k, "GetTimeZoneInformation", |c| {
        let p = c.arg(0);
        let _ = c.host.fill(p, 172, 0);
        ok(1, 0)
    });
    r.add(k, "SystemTimeToFileTime", |c| {
        let _ = c.host.write_u64(c.arg(1), 0x01DC_5A6B_0000_0000);
        ok(2, 1)
    });
    r.add(k, "FileTimeToSystemTime", |c| {
        write_systemtime(c, c.arg(1));
        ok(2, 1)
    });
    r.add(k, "FileTimeToLocalFileTime", |c| {
        let v = c.host.read_u64(c.arg(0)).unwrap_or(0);
        let _ = c.host.write_u64(c.arg(1), v);
        ok(2, 1)
    });
    r.add(k, "LocalFileTimeToFileTime", |c| {
        let v = c.host.read_u64(c.arg(0)).unwrap_or(0);
        let _ = c.host.write_u64(c.arg(1), v);
        ok(2, 1)
    });
    r.add(k, "FileTimeToDosDateTime", |c| {
        let _ = c.host.write_u16(c.arg(1), 0x5C21);
        let _ = c.host.write_u16(c.arg(2), 0);
        ok(3, 1)
    });
    r.add(k, "GetThreadTimes", |_c| ok(5, 1));
    r.add(k, "GetProcessTimes", |_c| ok(5, 1));
    // ---- files ----------------------------------------------------------------------
    r.add(k, "GetStdHandle", |c| ok(1, 0x10 + c.arg(0).wrapping_neg() * 4 % 12));
    r.add(k, "SetStdHandle", |_c| ok(2, 1));
    r.add(k, "GetFileType", |c| {
        let h = c.arg(0);
        ok(1, if matches!(c.p.handles.get(&h), Some(Handle::File(_))) { 1 } else { 2 })
    });
    r.add(k, "CreateFileA", |c| {
        let path = c.str_arg(0);
        let (access, disposition) = (c.arg(1), c.arg(4));
        let h = create_file(c, &path, access, disposition);
        c.trace(format!("({path:?}, access={access:#x}, disp={disposition}) = {h:#x}"));
        ok(7, h)
    });
    r.add(k, "CreateFileW", |c| {
        let path = c.wstr_arg(0);
        let (access, disposition) = (c.arg(1), c.arg(4));
        let h = create_file(c, &path, access, disposition);
        c.trace(format!("({path:?}, access={access:#x}, disp={disposition}) = {h:#x}"));
        ok(7, h)
    });
    r.add(k, "ReadFile", |c| {
        let (h, buf, n, out) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let r = read_file(c, h, buf, n);
        match r {
            Some(got) => {
                write_out(c, out, got);
                ok(5, 1)
            }
            None => {
                c.set_last_error(ERROR_INVALID_HANDLE);
                ok(5, 0)
            }
        }
    });
    r.add(k, "WriteFile", |c| {
        let (h, buf, n, out) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let mut data = vec![0u8; n as usize];
        let _ = c.host.read(buf, &mut data);
        if h < 0x100 {
            // std handles: log
            c.p.log(format!("stdout: {}", String::from_utf8_lossy(&data).trim_end()));
            write_out(c, out, n);
            return ok(5, 1);
        }
        let written = write_file(c, h, &data);
        write_out(c, out, written);
        ok(5, 1)
    });
    r.add(k, "SetFilePointer", |c| {
        let (h, lo, hi_ptr, method) = (c.arg(0), c.arg(1) as i32 as i64, c.arg(2), c.arg(3));
        let hi = if hi_ptr != 0 { c.host.read_u32(hi_ptr).unwrap_or(0) as i32 as i64 } else { if lo < 0 { -1 } else { 0 } };
        let dist = if hi_ptr != 0 { (hi << 32) | (lo as u32 as i64) } else { lo };
        match seek_file(c, h, dist, method) {
            Some(pos) => {
                if hi_ptr != 0 {
                    let _ = c.host.write_u32(hi_ptr, (pos >> 32) as u32);
                }
                ok(4, pos as u32)
            }
            None => ok(4, 0xFFFF_FFFF),
        }
    });
    r.add(k, "SetFilePointerEx", |c| {
        let (h, dist, out, method) = (c.arg(0), c.host.read_u64(c.regs.esp + 8).unwrap_or(0) as i64, c.arg(3), c.arg(4));
        match seek_file(c, h, dist, method) {
            Some(pos) => {
                if out != 0 {
                    let _ = c.host.write_u64(out, pos);
                }
                ok(5, 1)
            }
            None => ok(5, 0),
        }
    });
    r.add(k, "GetFileSize", |c| {
        let (h, hi) = (c.arg(0), c.arg(1));
        match file_size(c, h) {
            Some(size) => {
                if hi != 0 {
                    let _ = c.host.write_u32(hi, (size >> 32) as u32);
                }
                ok(2, size as u32)
            }
            None => ok(2, 0xFFFF_FFFF),
        }
    });
    r.add(k, "GetFileSizeEx", |c| {
        let (h, out) = (c.arg(0), c.arg(1));
        match file_size(c, h) {
            Some(size) => {
                let _ = c.host.write_u64(out, size);
                ok(2, 1)
            }
            None => ok(2, 0),
        }
    });
    r.add(k, "SetEndOfFile", |c| {
        let h = c.arg(0);
        let Some((key, pos)) = with_file(c, h, |fh| (fh.key.clone(), fh.pos)) else { return ok(1, 0) };
        let r = c.p.fs.set_len(&key, pos);
        ok(1, r as u32)
    });
    r.add(k, "FlushFileBuffers", |_c| ok(1, 1));
    r.add(k, "GetFileAttributesA", |c| {
        let path = c.str_arg(0);
        let r = match c.p.fs.stat(&c.p.vpath(&path)) {
            Some(m) => if m.is_dir { 0x10 } else { 0x80 },
            None => {
                c.set_last_error(ERROR_FILE_NOT_FOUND);
                0xFFFF_FFFF
            }
        };
        c.trace(format!("({path:?}) = {r:#x}"));
        ok(1, r)
    });
    r.add(k, "GetFileAttributesW", |c| {
        let path = c.wstr_arg(0);
        let r = match c.p.fs.stat(&c.p.vpath(&path)) {
            Some(m) => if m.is_dir { 0x10 } else { 0x80 },
            None => {
                c.set_last_error(ERROR_FILE_NOT_FOUND);
                0xFFFF_FFFF
            }
        };
        ok(1, r)
    });
    r.add(k, "SetFileAttributesA", |_c| ok(2, 1));
    r.add(k, "GetFileInformationByHandle", |c| {
        let h = c.arg(0);
        let size = file_size(c, h).unwrap_or(0);
        let p = c.arg(1);
        let _ = c.host.fill(p, 52, 0);
        let _ = c.host.write_u32(p, 0x80);
        let _ = c.host.write_u32(p + 32, (size >> 32) as u32);
        let _ = c.host.write_u32(p + 36, size as u32);
        let _ = c.host.write_u32(p + 40, 1);
        ok(2, 1)
    });
    r.add(k, "GetFileTime", |c| {
        for i in 1..4 {
            let p = c.arg(i);
            if p != 0 {
                let _ = c.host.write_u64(p, 0x01DC_5A6B_0000_0000);
            }
        }
        ok(4, 1)
    });
    r.add(k, "SetFileTime", |_c| ok(4, 1));
    r.add(k, "GetFullPathNameA", |c| {
        let path = c.str_arg(0);
        let full = if path.contains(':') { path.clone() } else { format!("{}\\{}", c.p.cwd, path) };
        let n = copy_str_out(c, c.arg(2), c.arg(1), full.as_bytes());
        let fp = c.arg(3);
        if fp != 0 {
            let pos = full.rfind('\\').map(|p| p + 1).unwrap_or(0);
            let _ = c.host.write_u32(fp, c.arg(2) + pos as u32);
        }
        ok(4, n)
    });
    r.add(k, "GetShortPathNameA", |c| {
        let path = c.str_arg(0);
        let n = copy_str_out(c, c.arg(1), c.arg(2), path.as_bytes());
        ok(3, n)
    });
    r.alias(k, "GetLongPathNameA", k, "GetShortPathNameA");
    r.add(k, "GetDriveTypeA", |_c| ok(1, 3));
    r.add(k, "GetDiskFreeSpaceA", |c| {
        write_out(c, c.arg(1), 8);
        write_out(c, c.arg(2), 512);
        write_out(c, c.arg(3), 0x0010_0000);
        write_out(c, c.arg(4), 0x0040_0000);
        ok(5, 1)
    });
    r.add(k, "GetDiskFreeSpaceExA", |c| {
        for i in 1..4 {
            let p = c.arg(i);
            if p != 0 {
                let _ = c.host.write_u64(p, 0x4_0000_0000);
            }
        }
        ok(4, 1)
    });
    r.add(k, "GetLogicalDrives", |_c| ok(0, 0x4));
    r.add(k, "GetVolumeInformationA", |c| {
        copy_str_out(c, c.arg(1), c.arg(2), b"GAME");
        write_out(c, c.arg(3), 0x1234_5678);
        write_out(c, c.arg(4), 255);
        write_out(c, c.arg(5), 0);
        copy_str_out(c, c.arg(6), c.arg(7), b"NTFS");
        ok(8, 1)
    });
    r.add(k, "FindFirstFileA", |c| {
        let pattern = c.str_arg(0);
        let h = find_first(c, &pattern);
        c.trace(format!("({pattern:?}) = {h:#x}"));
        if h == INVALID_HANDLE_VALUE {
            c.set_last_error(ERROR_FILE_NOT_FOUND);
            return ok(2, h);
        }
        if !find_next(c, h, c.arg(1)) {
            c.p.handles.remove(&h);
            c.set_last_error(ERROR_FILE_NOT_FOUND);
            return ok(2, INVALID_HANDLE_VALUE);
        }
        ok(2, h)
    });
    r.add(k, "FindNextFileA", |c| {
        let h = c.arg(0);
        if find_next(c, h, c.arg(1)) {
            ok(2, 1)
        } else {
            c.set_last_error(ERROR_NO_MORE_FILES);
            ok(2, 0)
        }
    });
    r.add(k, "FindClose", |c| {
        let h = c.arg(0);
        c.p.handles.remove(&h);
        ok(1, 1)
    });
    r.add(k, "CreateFileMappingA", |c| {
        let h = c.p.new_handle(Handle::Dummy("file mapping"));
        ok(6, h)
    });
    r.add(k, "MapViewOfFile", |_c| ok(5, 0));
    r.add(k, "UnmapViewOfFile", |_c| ok(1, 1));
    r.add(k, "GetConsoleMode", |_c| ok(2, 0));
    r.add(k, "GetConsoleCP", |_c| ok(0, 437));
    r.add(k, "GetConsoleOutputCP", |_c| ok(0, 437));
    r.add(k, "AllocConsole", |_c| ok(0, 1));
    r.add(k, "FreeConsole", |_c| ok(0, 1));
    r.add(k, "SetConsoleCtrlHandler", |_c| ok(2, 1));
    r.add(k, "WriteConsoleA", |c| {
        let (buf, n) = (c.arg(1), c.arg(2));
        let mut d = vec![0u8; n as usize];
        let _ = c.host.read(buf, &mut d);
        c.p.log(format!("console: {}", String::from_utf8_lossy(&d)));
        write_out(c, c.arg(3), n);
        ok(5, 1)
    });
    // ---- misc --------------------------------------------------------------------------
    r.add(k, "GetSystemPowerStatus", |_c| ok(1, 0));
    r.add(k, "Beep", |_c| ok(2, 1));
    r.add(k, "GetKeyboardType", |c| ok(1, match c.arg(0) { 0 => 4, 1 => 0, _ => 12 }));
    r.add(k, "FormatMessageA", |c| {
        let msg = format!("Error {}", c.arg(3));
        let n = copy_str_out(c, c.arg(4), c.arg(5), msg.as_bytes());
        ok(7, n)
    });
    r.add(k, "GetSystemMetrics", |_c| ok(1, 0));
    r.add(k, "GetNumberOfConsoleInputEvents", |_c| ok(2, 0));
    r.add(k, "QueryDosDeviceA", |_c| ok(3, 0));
    r.add(k, "DeviceIoControl", |_c| ok(8, 0));
    r.add(k, "GetOverlappedResult", |_c| ok(4, 0));
    r.add(k, "WinExec", |_c| ok(2, 0));
    r.add(k, "CreateProcessA", |_c| ok(10, 0));
    r.add(k, "GetProfileIntA", |c| ok(3, c.arg(2)));
    r.add(k, "GetPrivateProfileStringA", |c| {
        let default = c.str_arg(2);
        let n = copy_str_out(c, c.arg(3), c.arg(4), default.as_bytes());
        ok(6, n)
    });
    r.add(k, "GetPrivateProfileIntA", |c| ok(4, c.arg(2)));
    r.add(k, "WritePrivateProfileStringA", |_c| ok(4, 1));
    r.add(k, "GetProcessWorkingSetSize", |c| {
        write_out(c, c.arg(1), 0x0010_0000);
        write_out(c, c.arg(2), 0x0100_0000);
        ok(3, 1)
    });
    r.add(k, "SetProcessWorkingSetSize", |_c| ok(3, 1));
    r.add(k, "GetThreadContext", |_c| ok(2, 0));
    r.add(k, "SetThreadContext", |_c| ok(2, 0));
    r.add(k, "GetCurrentProcessorNumber", |_c| ok(0, 0));
    r.add(k, "EncodePointer", |c| ok(1, c.arg(0) ^ 0x5a5a_5a5a));
    r.add(k, "DecodePointer", |c| ok(1, c.arg(0) ^ 0x5a5a_5a5a));
    r.add(k, "FlsAlloc", |c| {
        let i = c.p.tls_used.iter().position(|u| !u).unwrap_or(0);
        c.p.tls_used[i] = true;
        ok(1, i as u32)
    });
    r.add(k, "FlsFree", |_c| ok(1, 1));
    r.add(k, "FlsGetValue", |c| {
        let i = c.arg(0);
        let v = c.host.read_u32(c.regs.fs_base + 0xE10 + 4 * i).unwrap_or(0);
        ok(1, v)
    });
    r.add(k, "FlsSetValue", |c| {
        let (i, v) = (c.arg(0), c.arg(1));
        let _ = c.host.write_u32(c.regs.fs_base + 0xE10 + 4 * i, v);
        ok(2, 1)
    });

    // ntdll pieces Delphi/MSVC runtimes import directly
    let n = "ntdll.dll";
    r.alias(n, "RtlUnwind", k, "RtlUnwind");
    r.add(n, "RtlGetVersion", |c| {
        let p = c.arg(0);
        let _ = c.host.write_u32(p + 4, 5);
        let _ = c.host.write_u32(p + 8, 1);
        let _ = c.host.write_u32(p + 12, 2600);
        let _ = c.host.write_u32(p + 16, 2);
        ok(1, 0)
    });
    r.add(n, "NtQueryInformationProcess", |_c| ok(5, 0xC000_0003u32));
    r.add(n, "RtlAllocateHeap", |c| {
        let (heap, flags, size) = (c.arg(0), c.arg(1), c.arg(2));
        let a = c.p.heap_alloc(c.host, heap, size, flags & 8 != 0);
        ok(3, a)
    });
    r.add(n, "RtlFreeHeap", |c| {
        let a = c.arg(2);
        c.p.heap_free(a);
        ok(3, 1)
    });
    r.add(n, "RtlReAllocateHeap", |c| {
        let (heap, flags, old, size) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let a = heap_realloc(c, heap, old, size, flags & 8 != 0);
        ok(4, a)
    });
    r.add(n, "RtlSizeHeap", |c| {
        let a = c.arg(2);
        ok(3, c.p.heap_size(a).unwrap_or(0))
    });
    r.add(n, "NtCurrentTeb", |c| ok(0, c.regs.fs_base));
    r.add(n, "RtlInitializeCriticalSection", |_c| ok(1, 0));
    r.add(n, "RtlEnterCriticalSection", |_c| ok(1, 0));
    r.add(n, "RtlLeaveCriticalSection", |_c| ok(1, 0));
    r.add(n, "RtlDeleteCriticalSection", |_c| ok(1, 0));
    r.add(n, "RtlNtStatusToDosError", |c| ok(1, c.arg(0) & 0xFFFF));
    let _ = ERROR_SUCCESS;
}

/// DLLs the HLE layer always provides, even if a file of that name exists.
const CORE_DLLS: &[&str] = &["kernel32", "user32", "gdi32", "ntdll", "advapi32", "ole32", "oleaut32", "shell32", "comctl32", "comdlg32", "winmm", "version", "imm32", "msimg32", "uxtheme", "dwmapi", "ws2_32", "wsock32", "mpr", "winspool.drv", "crypt32", "psapi", "shlwapi", "setupapi", "usp10", "wininet", "dbghelp", "olepro32", "imagehlp", "urlmon", "gdiplus", "windowscodecs", "hid"];
/// Further HLE DLLs, used when the game does not ship its own copy.
const HLE_DLLS: &[&str] = &["d3d8", "d3d9", "ddraw", "dsound", "dinput8", "dinput", "dplayx", "xinput1_3", "xinput1_4", "xinput9_1_0", "msvcrt"];

fn dll_stem(name: &str) -> String {
    let lower = name.to_lowercase();
    let base = lower.rsplit(['\\', '/']).next().unwrap_or(&lower).to_string();
    base.trim_end_matches(".dll").to_string()
}

fn module_handle(c: &mut Ctx, name: &str) -> u32 {
    if name.is_empty() {
        return c.p.modules.first().map(|m| m.base).unwrap_or(0x40_0000);
    }
    if let Some(m) = c.p.module_by_name(name.rsplit(['\\', '/']).next().unwrap_or(name)) {
        return m.base;
    }
    let stem = dll_stem(name);
    if CORE_DLLS.contains(&stem.as_str()) || HLE_DLLS.contains(&stem.as_str()) || c.p.fake_modules.contains_key(&format!("{stem}.dll")) {
        return c.p.fake_module(&stem);
    }
    0
}

fn module_path(c: &mut Ctx, base: u32) -> String {
    if base == 0 {
        return c.p.exe_path.clone();
    }
    if let Some(m) = c.p.module_by_base(base) {
        return m.path.clone();
    }
    if let Some(n) = c.p.fake_module_name(base) {
        return format!("C:\\Windows\\system32\\{n}");
    }
    c.p.exe_path.clone()
}

fn load_library(c: &mut Ctx, name: &str) -> Result<u32, ApiError> {
    if let Some(m) = c.p.module_by_name(name.rsplit(['\\', '/']).next().unwrap_or(name)) {
        return Ok(m.base);
    }
    let stem = dll_stem(name);
    if CORE_DLLS.contains(&stem.as_str()) {
        return Ok(c.p.fake_module(&stem));
    }
    // A DLL the game ships (or extracted at run time) is loaded and recompiled.
    let with_ext = if name.to_lowercase().ends_with(".dll") || name.contains('.') { name.to_string() } else { format!("{name}.dll") };
    let key = c.p.vpath(&with_ext);
    if c.p.fs.is_file(&key) {
        if let Some(bytes) = c.p.fs.read_all(&key) {
            let base_name = with_ext.rsplit(['\\', '/']).next().unwrap_or(&with_ext).to_string();
            let guest_path = if with_ext.contains(':') { with_ext.clone() } else { format!("{}\\{}", c.p.cwd.trim_end_matches('\\'), with_ext.trim_start_matches('\\')) };
            return load_image(c, &base_name, &guest_path, bytes, true);
        }
    }
    if HLE_DLLS.contains(&stem.as_str()) {
        return Ok(c.p.fake_module(&stem));
    }
    c.p.log(format!("LoadLibrary({name:?}): not found"));
    c.set_last_error(ERROR_MOD_NOT_FOUND);
    Ok(0)
}

/// Maps a PE image, binds its imports, runs TLS callbacks and DllMain. Shared by
/// LoadLibrary and the harness's static-import preloading.
pub fn load_image(c: &mut Ctx, base_name: &str, guest_path: &str, bytes: Vec<u8>, attach: bool) -> Result<u32, ApiError> {
    let req = crate::loader::LoadRequest { name: base_name, path: guest_path, bytes };
    let (base, entry, is_dll, tls) = {
        let p = &mut *c.p;
        let mut resolver = crate::StubResolver { registry: &mut p.registry, modules: &p.modules };
        let g: &mut dyn Guest = &mut *c.host;
        let m = crate::loader::load(req, &mut p.vm, g, &mut resolver).map_err(ApiError)?;
        p.registry.flush_data(g);
        let r = (m.base, m.entry, m.is_dll, m.tls_callbacks.clone());
        p.modules.push(m);
        r
    };
    c.p.log(format!("loaded {base_name} at {base:#x} (entry {entry:#x})"));
    if attach && is_dll {
        for cb in tls {
            c.call_guest(cb, &[base, 1, 0])?;
        }
        if entry != base {
            let r = c.call_guest(entry, &[base, 1, 0])?;
            if r == 0 {
                c.p.log(format!("{base_name}: DllMain returned FALSE"));
                c.set_last_error(1114);
                return Ok(0);
            }
        }
    }
    Ok(base)
}

fn proc_address(c: &mut Ctx, base: u32, name: &str) -> u32 {
    if let Some(m) = c.p.module_by_base(base) {
        return m.exports.get(name).copied().unwrap_or(0);
    }
    let dll = match c.p.fake_module_name(base) {
        Some(d) => d.to_string(),
        None => return 0,
    };
    let id = c.p.registry.intern(&dll, name);
    Process::stub_addr(id)
}

fn virtual_alloc(c: &mut Ctx, addr: u32, size: u32, ty: u32, protect: u32) -> u32 {
    let protect = if protect == 0 { PAGE_READWRITE } else { protect };
    if ty & MEM_COMMIT != 0 && addr != 0 {
        // commit inside an existing reservation, or reserve+commit at that address
        if c.p.vm.commit(addr, size, protect) {
            let _ = c.host.ensure_mapped(addr & !0xFFF, size + (addr & 0xFFF));
            return addr & !0xFFF;
        }
    }
    let commit = ty & MEM_COMMIT != 0;
    match c.p.vm.alloc(addr, size, commit, protect, ty & MEM_TOP_DOWN != 0, "VirtualAlloc") {
        Some(base) => {
            if c.host.ensure_mapped(base, size.div_ceil(0x1000) * 0x1000).is_err() {
                return 0;
            }
            if commit {
                let _ = c.host.fill(base, size.div_ceil(0x1000) * 0x1000, 0);
            }
            base
        }
        None => {
            if ty & MEM_RESERVE == 0 && ty & MEM_COMMIT != 0 && addr != 0 {
                // commit into free space at a specific address is also legal
                if let Some(base) = c.p.vm.alloc(addr, size, true, protect, false, "VirtualAlloc") {
                    let _ = c.host.ensure_mapped(base, size.div_ceil(0x1000) * 0x1000);
                    return base;
                }
            }
            0
        }
    }
}

fn heap_realloc(c: &mut Ctx, heap: u32, old: u32, size: u32, zero: bool) -> u32 {
    if old == 0 {
        return c.p.heap_alloc(c.host, heap, size, zero);
    }
    let old_size = c.p.heap_size(old).unwrap_or(0);
    let a = c.p.heap_alloc(c.host, heap, size, zero);
    if a != 0 {
        let n = old_size.min(size);
        let mut buf = vec![0u8; n as usize];
        let _ = c.host.read(old, &mut buf);
        let _ = c.host.write(a, &buf);
        c.p.heap_free(old);
    }
    a
}

fn string_const(c: &mut Ctx, key: &'static str, s: &str) -> u32 {
    if let Some(&a) = c.p.registry_values.get(key).and_then(|v| v.first()).map(|_| &0u32) {
        let _ = a;
    }
    let a = c.p.heap_alloc(c.host, 0, s.len() as u32 + 1, false);
    let _ = c.host.write_cstr(a, s.as_bytes());
    a
}

fn wstring_const(c: &mut Ctx, _key: &'static str, s: &str) -> u32 {
    let a = c.p.heap_alloc(c.host, 0, s.encode_utf16().count() as u32 * 2 + 2, false);
    let _ = c.host.write_wstr(a, s);
    a
}

fn locale_info(lctype: u32) -> &'static str {
    match lctype {
        0x1 => "0409",    // LOCALE_ILANGUAGE
        0x2 => "English (United States)",
        0x3 => "ENU",
        0x4 => "English",
        0x5 => "United States",
        0x6 => "USA",
        0x1002 => "English",
        0x1001 => "English",
        0x59 => "ENU",
        0xE => ".",       // LOCALE_SDECIMAL
        0xF => ",",       // LOCALE_STHOUSAND
        0x10 => "3;0",    // LOCALE_SGROUPING
        0x11 => "2",      // LOCALE_IDIGITS
        0x12 => "1",      // LOCALE_ILZERO
        0x13 => "1",      // LOCALE_INEGNUMBER
        0x14 => "$",      // LOCALE_SCURRENCY
        0x15 => "USD",
        0x16 => ".",      // LOCALE_SMONDECIMALSEP
        0x17 => ",",      // LOCALE_SMONTHOUSANDSEP
        0x18 => "3;0",
        0x19 => "2",      // LOCALE_ICURRDIGITS
        0x1B => "0",      // LOCALE_ICURRENCY
        0x1C => "0",      // LOCALE_INEGCURR
        0x1D => "/",      // LOCALE_SDATE
        0x1E => ":",      // LOCALE_STIME
        0x1F => "M/d/yyyy", // LOCALE_SSHORTDATE
        0x20 => "dddd, MMMM d, yyyy", // LOCALE_SLONGDATE
        0x21 => "0",      // LOCALE_IDATE
        0x22 => "0",      // LOCALE_ILDATE
        0x23 => "0",      // LOCALE_ITIME
        0x25 => "0",      // LOCALE_ITLZERO
        0x27 => "0",      // LOCALE_IDAYLZERO? (ICENTURY)
        0x28 => "AM",
        0x29 => "PM",
        0x1003 => "h:mm:ss tt", // LOCALE_STIMEFORMAT
        0x100B => "1252", // LOCALE_IDEFAULTANSICODEPAGE
        0xB => "437",     // LOCALE_IDEFAULTCODEPAGE
        0x2A => "1",
        0x2B..=0x36 => "",
        0x1004 => "1",    // LOCALE_IFIRSTDAYOFWEEK? (6=Sunday?) keep
        0x1005 => "0",
        0x1009 => "1",    // LOCALE_ICALENDARTYPE
        _ => "",
    }
}

fn ctype1(b: u8) -> u16 {
    let mut t = 0u16;
    if b.is_ascii_uppercase() { t |= 0x1 | 0x100; }
    if b.is_ascii_lowercase() { t |= 0x2 | 0x100; }
    if b.is_ascii_digit() { t |= 0x4; }
    if b == b' ' || (9..=13).contains(&b) { t |= 0x8; }
    if b.is_ascii_punctuation() { t |= 0x10; }
    if b.is_ascii_control() { t |= 0x20; }
    if b == b' ' { t |= 0x40; }
    if b.is_ascii_hexdigit() { t |= 0x80; }
    t
}

fn read_sized(c: &Ctx, addr: u32, n: i32) -> Vec<u8> {
    if n < 0 {
        c.host.read_cstr(addr).unwrap_or_default()
    } else {
        let mut b = vec![0u8; n as usize];
        let _ = c.host.read(addr, &mut b);
        b
    }
}

fn read_wsized(c: &Ctx, addr: u32, n: i32) -> String {
    if n < 0 {
        c.host.read_wstr(addr).unwrap_or_default()
    } else {
        let mut units = Vec::with_capacity(n as usize);
        for i in 0..n as u32 {
            units.push(c.host.read_u16(addr + 2 * i).unwrap_or(0));
        }
        String::from_utf16_lossy(&units)
    }
}

fn decode_cp(cp: u32, bytes: &[u8]) -> String {
    match cp {
        65001 => String::from_utf8_lossy(bytes).into_owned(),
        932 => encoding_rs::SHIFT_JIS.decode(bytes).0.into_owned(),
        _ => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

pub(crate) fn encode_cp(cp: u32, s: &str) -> Vec<u8> {
    match cp {
        65001 => s.as_bytes().to_vec(),
        932 => encoding_rs::SHIFT_JIS.encode(s).0.into_owned(),
        _ => encoding_rs::WINDOWS_1252.encode(s).0.into_owned(),
    }
}

fn write_systemtime(c: &mut Ctx, p: u32) {
    let secs = c.p.clock_us / 1_000_000;
    let fields: [u16; 8] = [2026, 1, 4, 1, (12 + secs / 3600 % 12) as u16, (secs / 60 % 60) as u16, (secs % 60) as u16, ((c.p.clock_us / 1000) % 1000) as u16];
    for (i, f) in fields.iter().enumerate() {
        let _ = c.host.write_u16(p + 2 * i as u32, *f);
    }
}

pub(crate) fn create_file(c: &mut Ctx, path: &str, access: u32, disposition: u32) -> u32 {
    let writable = access & 0x4000_0000 != 0;
    let key = c.p.vpath(path);
    // CREATE_NEW 1, CREATE_ALWAYS 2, OPEN_EXISTING 3, OPEN_ALWAYS 4, TRUNCATE_EXISTING 5
    let exists = c.p.fs.exists(&key);
    if !exists && !key.is_empty() && !c.p.fs.is_dir(crate::vfs::parent(&key)) {
        c.set_last_error(super::ERROR_PATH_NOT_FOUND);
        return INVALID_HANDLE_VALUE;
    }
    let ok_ = match disposition {
        3 | 5 if !exists => false,
        1 if exists => false,
        2 | 5 => c.p.fs.create(&key),
        1 | 4 if !exists => c.p.fs.create(&key),
        _ => true,
    };
    if !ok_ {
        c.set_last_error(if disposition == 1 && exists { super::ERROR_ALREADY_EXISTS } else { ERROR_FILE_NOT_FOUND });
        return INVALID_HANDLE_VALUE;
    }
    // ERROR_ALREADY_EXISTS is reported alongside success.
    c.set_last_error(if exists && matches!(disposition, 2 | 4) { super::ERROR_ALREADY_EXISTS } else { 0 });
    if c.p.fs.is_dir(&key) {
        return c.p.new_handle(Handle::Dummy("directory"));
    }
    c.p.new_handle(Handle::File(FileHandle { path: path.to_string(), key, pos: 0, writable }))
}

fn with_file<T>(c: &mut Ctx, h: u32, f: impl FnOnce(&mut FileHandle) -> T) -> Option<T> {
    match c.p.handles.get_mut(&h) {
        Some(Handle::File(fh)) => Some(f(fh)),
        _ => None,
    }
}

pub(crate) fn read_file(c: &mut Ctx, h: u32, buf: u32, n: u32) -> Option<u32> {
    let (key, pos) = with_file(c, h, |fh| (fh.key.clone(), fh.pos))?;
    let mut data = vec![0u8; n as usize];
    let got = c.p.fs.read_at(&key, pos, &mut data)?;
    let _ = c.host.write(buf, &data[..got]);
    with_file(c, h, |fh| fh.pos += got as u64);
    Some(got as u32)
}

pub(crate) fn write_file(c: &mut Ctx, h: u32, data: &[u8]) -> u32 {
    let Some((key, pos, writable)) = with_file(c, h, |fh| (fh.key.clone(), fh.pos, fh.writable)) else { return 0 };
    if !writable || !c.p.fs.write_at(&key, pos, data) {
        return 0;
    }
    with_file(c, h, |fh| fh.pos += data.len() as u64);
    data.len() as u32
}

pub(crate) fn seek_file(c: &mut Ctx, h: u32, dist: i64, method: u32) -> Option<u64> {
    let size = file_size(c, h)?;
    with_file(c, h, |fh| {
        let base = match method { 0 => 0i64, 1 => fh.pos as i64, _ => size as i64 };
        let np = (base + dist).max(0) as u64;
        fh.pos = np;
        np
    })
}

pub(crate) fn file_size(c: &mut Ctx, h: u32) -> Option<u64> {
    let key = with_file(c, h, |fh| fh.key.clone())?;
    c.p.fs.stat(&key).map(|m| m.size)
}

fn find_first(c: &mut Ctx, pattern: &str) -> u32 {
    let (dir, pat) = match pattern.rsplit_once(['\\', '/']) {
        Some((d, p)) => (d.to_string(), p.to_string()),
        None => (c.p.cwd.clone(), pattern.to_string()),
    };
    let mut entries = Vec::new();
    let dkey = c.p.vpath(&dir);
    for (name, md) in c.p.fs.list(&dkey) {
        if glob_match(&pat.to_lowercase(), &name.to_lowercase()) {
            entries.push(FindEntry { name, size: md.size, is_dir: md.is_dir });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if entries.is_empty() {
        return INVALID_HANDLE_VALUE;
    }
    c.p.new_handle(Handle::FindFile { entries, pos: 0 })
}

fn find_next(c: &mut Ctx, h: u32, out: u32) -> bool {
    let entry = match c.p.handles.get_mut(&h) {
        Some(Handle::FindFile { entries, pos }) => {
            if *pos >= entries.len() {
                return false;
            }
            let e = entries[*pos].clone();
            *pos += 1;
            e
        }
        _ => return false,
    };
    // WIN32_FIND_DATAA
    let _ = c.host.fill(out, 320, 0);
    let _ = c.host.write_u32(out, if entry.is_dir { 0x10 } else { 0x80 });
    let _ = c.host.write_u32(out + 28, (entry.size >> 32) as u32);
    let _ = c.host.write_u32(out + 32, entry.size as u32);
    let name = encode_cp(1252, &entry.name);
    let n = name.len().min(259);
    let _ = c.host.write(out + 44, &name[..n]);
    let _ = c.host.write_u8(out + 44 + n as u32, 0);
    true
}

fn glob_match(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            (Some('.'), None) => go(&p[1..], n),
            _ => false,
        }
    }
    go(&p, &n)
}

pub fn ret_to_host_addr() -> u32 {
    RET_TO_HOST
}

pub const _INSUFFICIENT: u32 = ERROR_INSUFFICIENT_BUFFER;

// ---- structured exception handling (x86 frame chain at fs:[0]) -----------------

pub const EXCEPTION_RECORD_SIZE: u32 = 80;
pub const CONTEXT_SIZE: u32 = 0x2CC;
pub const EXCEPTION_UNWINDING: u32 = 2;
pub const EXCEPTION_EXIT_UNWIND: u32 = 4;
pub const STATUS_UNWIND: u32 = 0xC000_0027;

fn write_exception_record(c: &mut Ctx, p: u32, code: u32, flags: u32, addr: u32, params: &[u32]) {
    let _ = c.host.fill(p, EXCEPTION_RECORD_SIZE, 0);
    let _ = c.host.write_u32(p, code);
    let _ = c.host.write_u32(p + 4, flags);
    let _ = c.host.write_u32(p + 12, addr);
    let _ = c.host.write_u32(p + 16, params.len() as u32);
    for (i, v) in params.iter().enumerate() {
        let _ = c.host.write_u32(p + 20 + 4 * i as u32, *v);
    }
}

fn write_context(c: &mut Ctx, p: u32, r: &Regs) {
    let _ = c.host.fill(p, CONTEXT_SIZE, 0);
    let _ = c.host.write_u32(p, 0x0001_0007); // CONTEXT_FULL
    let _ = c.host.write_u32(p + 0x1C, 0x027F); // FloatSave.ControlWord
    let _ = c.host.write_u32(p + 0x24, 0xFFFF); // FloatSave.TagWord
    let _ = c.host.write_u32(p + 0x90, 0x3B);
    let _ = c.host.write_u32(p + 0x94, 0x23);
    let _ = c.host.write_u32(p + 0x98, 0x23);
    let _ = c.host.write_u32(p + 0x9C, r.edi);
    let _ = c.host.write_u32(p + 0xA0, r.esi);
    let _ = c.host.write_u32(p + 0xA4, r.ebx);
    let _ = c.host.write_u32(p + 0xA8, r.edx);
    let _ = c.host.write_u32(p + 0xAC, r.ecx);
    let _ = c.host.write_u32(p + 0xB0, r.eax);
    let _ = c.host.write_u32(p + 0xB4, r.ebp);
    let _ = c.host.write_u32(p + 0xB8, r.eip);
    let _ = c.host.write_u32(p + 0xBC, 0x1B);
    let _ = c.host.write_u32(p + 0xC0, 0x202);
    let _ = c.host.write_u32(p + 0xC4, r.esp);
    let _ = c.host.write_u32(p + 0xC8, 0x23);
}

fn read_context(c: &Ctx, p: u32, r: &mut Regs) {
    let g = |off: u32| c.host.read_u32(p + off).unwrap_or(0);
    r.edi = g(0x9C);
    r.esi = g(0xA0);
    r.ebx = g(0xA4);
    r.edx = g(0xA8);
    r.ecx = g(0xAC);
    r.eax = g(0xB0);
    r.ebp = g(0xB4);
    r.eip = g(0xB8);
    r.esp = g(0xC4);
}

/// Walks the handler chain for a software or hardware exception whose context is
/// `ctx_regs`. A handler that returns ExceptionContinueExecution hands back a
/// (possibly rewritten) context, which becomes the new register state.
pub fn dispatch_exception(c: &mut Ctx, code: u32, flags: u32, addr: u32, params: &[u32], ctx_regs: &Regs) -> ApiResult {
    let teb = c.regs.fs_base;
    let saved_esp = c.regs.esp;
    let scratch = (c.regs.esp.min(ctx_regs.esp).wrapping_sub(0x40 + EXCEPTION_RECORD_SIZE + CONTEXT_SIZE + 16)) & !0xF;
    let rec = scratch;
    let ctx = scratch + EXCEPTION_RECORD_SIZE;
    let disp = ctx + CONTEXT_SIZE;
    write_exception_record(c, rec, code, flags & !EXCEPTION_UNWINDING, addr, params);
    write_context(c, ctx, ctx_regs);
    let _ = c.host.write_u32(disp, 0);
    let detail = if code == 0x0EED_FADE && params.len() >= 2 { format!(" ({})", delphi_exception_text(c, params[1])) } else { String::new() };
    c.p.log(format!("exception {code:#x} at {addr:#x}{detail}: dispatching"));
    // Nested callbacks must not overwrite the records.
    c.regs.esp = scratch;
    let mut frame = c.host.read_u32(teb).unwrap_or(0xFFFF_FFFF);
    let mut hops = 0;
    while frame != 0xFFFF_FFFF && frame != 0 {
        hops += 1;
        if hops > 10_000 {
            return Err(ApiError("SEH chain loops".into()));
        }
        let next = c.host.read_u32(frame).unwrap_or(0xFFFF_FFFF);
        let handler = c.host.read_u32(frame + 4).unwrap_or(0);
        if handler == 0 {
            break;
        }
        c.trace(format!(": frame {frame:#x} handler {handler:#x}"));
        let d = c.call_guest(handler, &[rec, frame, ctx, disp])?;
        match d {
            0 => {
                let mut regs = *c.regs;
                read_context(c, ctx, &mut regs);
                c.p.log(format!("exception {code:#x}: handler continues at {:#x} (esp {:#x} ebp {:#x})", regs.eip, regs.esp, regs.ebp));
                *c.regs = regs;
                return Ok(Ret::resume());
            }
            1 => {}
            other => c.p.log(format!("exception {code:#x}: handler {handler:#x} returned {other}")),
        }
        frame = next;
    }
    c.regs.esp = saved_esp;
    Err(ApiError(format!("unhandled exception {code:#x} at {addr:#x}")))
}

/// Class name and message of a Delphi exception object (TObject VMT layout of
/// Delphi 2009+: class name at vmt-44, FMessage at +4 as UnicodeString; falls
/// back to AnsiString).
fn delphi_exception_text(c: &Ctx, obj: u32) -> String {
    let vmt = c.host.read_u32(obj).unwrap_or(0);
    // vmtClassName is at -56 since Delphi 2009 and at -44 before (Delphi 7 builds GM8.0).
    let mut name = String::new();
    for off in [56u32, 44] {
        let name_p = c.host.read_u32(vmt.wrapping_sub(off)).unwrap_or(0);
        if name_p == 0 {
            continue;
        }
        let n = c.host.read_u8(name_p).unwrap_or(0) as u32;
        let mut b = vec![0u8; n as usize];
        let _ = c.host.read(name_p + 1, &mut b);
        if n > 0 && b.iter().all(|ch| ch.is_ascii_alphanumeric() || *ch == b'_') {
            name = String::from_utf8_lossy(&b).into_owned();
            break;
        }
    }
    let msg_p = c.host.read_u32(obj + 4).unwrap_or(0);
    let mut msg = String::new();
    if msg_p != 0 {
        let len = c.host.read_u32(msg_p - 4).unwrap_or(0).min(4096);
        let elem = c.host.read_u16(msg_p - 10).unwrap_or(1);
        if elem == 2 {
            let units: Vec<u16> = (0..len).map(|i| c.host.read_u16(msg_p + 2 * i).unwrap_or(0)).collect();
            msg = String::from_utf16_lossy(&units);
        } else {
            let mut b = vec![0u8; len as usize];
            let _ = c.host.read(msg_p, &mut b);
            msg = String::from_utf8_lossy(&b).into_owned();
        }
    }
    format!("{name}: {}", msg.trim_end())
}

/// RtlUnwind: calls every handler between the top of the chain and `target` with
/// EXCEPTION_UNWINDING, popping frames as it goes.
pub fn unwind_frames(c: &mut Ctx, target: u32, rec_in: u32) -> Result<(), ApiError> {
    let teb = c.regs.fs_base;
    let saved = *c.regs;
    let scratch = (c.regs.esp.wrapping_sub(0x40 + EXCEPTION_RECORD_SIZE + CONTEXT_SIZE + 16)) & !0xF;
    let rec = if rec_in != 0 {
        rec_in
    } else {
        write_exception_record(c, scratch, STATUS_UNWIND, 0, c.regs.eip, &[]);
        scratch
    };
    let ctx = scratch + EXCEPTION_RECORD_SIZE;
    let disp = ctx + CONTEXT_SIZE;
    write_context(c, ctx, &saved);
    let _ = c.host.write_u32(disp, 0);
    let flags = c.host.read_u32(rec + 4).unwrap_or(0) | EXCEPTION_UNWINDING | if target == 0 { EXCEPTION_EXIT_UNWIND } else { 0 };
    let _ = c.host.write_u32(rec + 4, flags);
    c.regs.esp = scratch;
    let mut frame = c.host.read_u32(teb).unwrap_or(0xFFFF_FFFF);
    let mut hops = 0;
    while frame != 0xFFFF_FFFF && frame != 0 && frame != target {
        hops += 1;
        if hops > 10_000 {
            return Err(ApiError("SEH chain loops during unwind".into()));
        }
        let next = c.host.read_u32(frame).unwrap_or(0xFFFF_FFFF);
        let handler = c.host.read_u32(frame + 4).unwrap_or(0);
        if handler != 0 {
            c.trace(format!(": unwinding frame {frame:#x} handler {handler:#x}"));
            c.call_guest(handler, &[rec, frame, ctx, disp])?;
        }
        let _ = c.host.write_u32(teb, next);
        frame = next;
    }
    let _ = c.host.write_u32(teb, if target == 0 { 0xFFFF_FFFF } else { target });
    *c.regs = saved;
    Ok(())
}

// ---- Unicode variants, resources, mappings, formatting (second batch) --------------

fn wstr(c: &Ctx, a: u32) -> String {
    if a == 0 { String::new() } else { c.host.read_wstr(a).unwrap_or_default() }
}



fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// SYSTEMTIME fields (year, month, weekday, day, hour, minute, second, ms) for the
/// host clock plus the virtual clock offset.
pub fn host_systemtime(c: &Ctx) -> [u16; 8] {
    let now = c.p.epoch_unix + c.p.clock_us / 1_000_000;
    let days = (now / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let dow = ((days + 4) % 7) as u16;
    let s = now % 86_400;
    [y as u16, m as u16, dow, d as u16, (s / 3600) as u16, (s / 60 % 60) as u16, (s % 60) as u16, ((c.p.clock_us / 1000) % 1000) as u16]
}

fn read_systemtime(c: &Ctx, p: u32) -> [u16; 8] {
    if p == 0 {
        return host_systemtime(c);
    }
    let mut st = [0u16; 8];
    for (i, f) in st.iter_mut().enumerate() {
        *f = c.host.read_u16(p + 2 * i as u32).unwrap_or(0);
    }
    st
}

/// Windows date/time picture formatting (d, dd, ddd, dddd, M.., y.., h, H, m, s, t, g, 'literal').
pub fn format_picture(fmt: &str, st: &[u16; 8]) -> String {
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let (y, mo, dow, d, h, mi, s) = (st[0] as u32, st[1].clamp(1, 12) as usize, (st[2] % 7) as usize, st[3] as u32, st[4] as u32, st[5] as u32, st[6] as u32);
    let h12 = if h % 12 == 0 { 12 } else { h % 12 };
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                out.push(chars[i]);
                i += 1;
            }
            i += 1;
            continue;
        }
        if "dMyhHmstg".contains(ch) {
            let mut n = 1;
            while i + n < chars.len() && chars[i + n] == ch {
                n += 1;
            }
            let piece = match (ch, n) {
                ('d', 1) => d.to_string(),
                ('d', 2) => format!("{d:02}"),
                ('d', 3) => DAYS[dow][..3].to_string(),
                ('d', _) => DAYS[dow].to_string(),
                ('M', 1) => mo.to_string(),
                ('M', 2) => format!("{mo:02}"),
                ('M', 3) => MONTHS[mo - 1][..3].to_string(),
                ('M', _) => MONTHS[mo - 1].to_string(),
                ('y', 1) => (y % 100).to_string(),
                ('y', 2) => format!("{:02}", y % 100),
                ('y', _) => format!("{y}"),
                ('h', 1) => h12.to_string(),
                ('h', _) => format!("{h12:02}"),
                ('H', 1) => h.to_string(),
                ('H', _) => format!("{h:02}"),
                ('m', 1) => mi.to_string(),
                ('m', _) => format!("{mi:02}"),
                ('s', 1) => s.to_string(),
                ('s', _) => format!("{s:02}"),
                ('t', 1) => if h < 12 { "A" } else { "P" }.to_string(),
                ('t', _) => if h < 12 { "AM" } else { "PM" }.to_string(),
                ('g', _) => "A.D.".to_string(),
                _ => String::new(),
            };
            out.push_str(&piece);
            i += n;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

fn date_time_format(c: &mut Ctx, wide: bool, time: bool) -> ApiResult {
    let flags = c.arg(1);
    let st = read_systemtime(c, c.arg(2));
    let fmt_p = c.arg(3);
    let fmt = if fmt_p == 0 {
        if time {
            if flags & 8 != 0 { "HH:mm:ss" } else if flags & 2 != 0 { "h:mm tt" } else { "h:mm:ss tt" }.to_string()
        } else if flags & 2 != 0 {
            "dddd, MMMM dd, yyyy".to_string()
        } else {
            "M/d/yyyy".to_string()
        }
    } else if wide {
        wstr(c, fmt_p)
    } else {
        c.host.read_cstr_lossy(fmt_p)
    };
    let s = format_picture(&fmt, &st);
    let (out, cch) = (c.arg(4), c.arg(5));
    if cch == 0 {
        return ok(6, s.len() as u32 + 1);
    }
    let n = if wide { copy_wstr_out(c, out, cch, &s) } else { copy_str_out(c, out, cch, s.as_bytes()) };
    ok(6, n + 1)
}

fn system_message(code: u32) -> String {
    let s = match code {
        0 => "The operation completed successfully.",
        1 => "Incorrect function.",
        2 => "The system cannot find the file specified.",
        3 => "The system cannot find the path specified.",
        5 => "Access is denied.",
        6 => "The handle is invalid.",
        8 => "Not enough storage is available to process this command.",
        32 => "The process cannot access the file because it is being used by another process.",
        87 => "The parameter is incorrect.",
        112 => "There is not enough space on the disk.",
        120 => "This function is not supported on this system.",
        122 => "The data area passed to a system call is too small.",
        123 => "The filename, directory name, or volume label syntax is incorrect.",
        126 => "The specified module could not be found.",
        127 => "The specified procedure could not be found.",
        183 => "Cannot create a file when that file already exists.",
        203 => "The system could not find the environment option that was entered.",
        1400 => "Invalid window handle.",
        _ => return format!("Unknown error {code} ({code:#x})."),
    };
    s.to_string()
}

fn format_message(c: &mut Ctx, wide: bool) -> ApiResult {
    let flags = c.arg(0);
    let source = c.arg(1);
    let msgid = c.arg(2);
    let buf = c.arg(4);
    let size = c.arg(5);
    let mut text = if flags & 0x400 != 0 {
        if wide { wstr(c, source) } else { c.host.read_cstr_lossy(source) }
    } else {
        system_message(msgid)
    };
    if flags & 0x400 == 0 {
        text.push_str("\r\n");
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let bytes = if wide { (units.len() as u32 + 1) * 2 } else { text.len() as u32 + 1 };
    if flags & 0x100 != 0 {
        let heap = c.p.process_heap;
        let g: &mut dyn Guest = &mut *c.host;
        let mem = c.p.heap_alloc(g, heap, bytes.max(size), true);
        if mem == 0 {
            return ok(7, 0);
        }
        if wide {
            let _ = c.host.write_wstr(mem, &text);
        } else {
            let _ = c.host.write_cstr(mem, text.as_bytes());
        }
        let _ = c.host.write_u32(buf, mem);
        return ok(7, if wide { units.len() as u32 } else { text.len() as u32 });
    }
    if buf == 0 || size == 0 {
        return ok(7, 0);
    }
    let n = if wide { copy_wstr_out(c, buf, size, &text) } else { copy_str_out(c, buf, size, text.as_bytes()) };
    ok(7, n)
}

fn expand_env(c: &Ctx, s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match c.p.env.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn full_path(c: &Ctx, name: &str) -> String {
    let n = name.replace('/', "\\");
    if n.len() >= 2 && n.as_bytes()[1] == b':' || n.starts_with("\\\\") {
        n
    } else if let Some(stripped) = n.strip_prefix('\\') {
        format!("{}\\{}", &c.p.cwd[..2], stripped)
    } else {
        format!("{}\\{}", c.p.cwd.trim_end_matches('\\'), n)
    }
}

fn find_next_w(c: &mut Ctx, h: u32, out: u32) -> bool {
    let entry = match c.p.handles.get_mut(&h) {
        Some(Handle::FindFile { entries, pos }) => {
            if *pos >= entries.len() {
                return false;
            }
            let e = entries[*pos].clone();
            *pos += 1;
            e
        }
        _ => return false,
    };
    let _ = c.host.fill(out, 592, 0);
    let _ = c.host.write_u32(out, if entry.is_dir { 0x10 } else { 0x80 });
    let _ = c.host.write_u32(out + 28, (entry.size >> 32) as u32);
    let _ = c.host.write_u32(out + 32, entry.size as u32);
    let units: Vec<u16> = entry.name.encode_utf16().take(259).collect();
    for (i, u) in units.iter().enumerate() {
        let _ = c.host.write_u16(out + 44 + 2 * i as u32, *u);
    }
    true
}

pub enum ResArg {
    Id(u32),
    Name(String),
}

fn res_arg(c: &Ctx, a: u32, wide: bool) -> ResArg {
    if a < 0x10000 {
        ResArg::Id(a)
    } else if wide {
        let s = wstr(c, a);
        if let Some(n) = s.strip_prefix('#') { n.parse().map(ResArg::Id).unwrap_or(ResArg::Name(s.clone())) } else { ResArg::Name(s) }
    } else {
        let s = c.host.read_cstr_lossy(a);
        if let Some(n) = s.strip_prefix('#') { n.parse().map(ResArg::Id).unwrap_or(ResArg::Name(s.clone())) } else { ResArg::Name(s) }
    }
}

fn res_match(id: &pe::ResId, a: &ResArg) -> bool {
    match (id, a) {
        (pe::ResId::Id(x), ResArg::Id(y)) => x == y,
        (pe::ResId::Name(x), ResArg::Name(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// Locates a resource in a mapped image: (guest address, size).
pub fn find_resource(c: &mut Ctx, hmod: u32, kind: &ResArg, name: &ResArg) -> Option<(u32, u32)> {
    let base = if hmod == 0 { c.p.modules.first().map(|m| m.base).unwrap_or(0) } else { hmod };
    let m = c.p.module_by_base(base)?;
    let pe = pe::Pe::parse(&m.file).ok()?;
    let res = pe.resources().ok()?;
    let r = res.iter().find(|r| res_match(&r.kind, kind) && res_match(&r.name, name))?;
    Some((base + r.rva, r.size))
}

/// String-table lookup (RT_STRING blocks of 16 length-prefixed UTF-16 strings).
pub fn load_string(c: &mut Ctx, hmod: u32, id: u32) -> Option<(u32, Vec<u16>)> {
    let (addr, size) = find_resource(c, hmod, &ResArg::Id(6), &ResArg::Id(id / 16 + 1))?;
    let mut p = addr;
    let end = addr + size;
    for i in 0..16 {
        if p + 2 > end {
            return None;
        }
        let len = c.host.read_u16(p).unwrap_or(0) as u32;
        if i == id % 16 {
            let units: Vec<u16> = (0..len).map(|k| c.host.read_u16(p + 2 + 2 * k).unwrap_or(0)).collect();
            return Some((p + 2, units));
        }
        p += 2 + 2 * len;
    }
    None
}

fn find_resource_api(c: &mut Ctx, wide: bool, ex: bool) -> ApiResult {
    let hmod = c.arg(0);
    let (kind, name) = if ex { (res_arg(c, c.arg(1), wide), res_arg(c, c.arg(2), wide)) } else { (res_arg(c, c.arg(2), wide), res_arg(c, c.arg(1), wide)) };
    let n = if ex { 4 } else { 3 };
    match find_resource(c, hmod, &kind, &name) {
        Some((addr, size)) => {
            let h = c.p.new_handle(Handle::Resource { addr, size });
            c.trace(format!(" = {h:#x} ({size} bytes at {addr:#x})"));
            ok(n, h)
        }
        None => {
            c.trace(": not found".into());
            c.set_last_error(1814);
            ok(n, 0)
        }
    }
}

fn write_attr_data(c: &mut Ctx, out: u32, md: &crate::vfs::Meta) {
    let _ = c.host.write_u32(out, if md.is_dir { 0x10 } else { 0x80 });
    let _ = c.host.write_u64(out + 4, md.mtime);
    let _ = c.host.write_u64(out + 12, md.mtime);
    let _ = c.host.write_u64(out + 20, md.mtime);
    let _ = c.host.write_u32(out + 28, (md.size >> 32) as u32);
    let _ = c.host.write_u32(out + 32, md.size as u32);
}

fn copy_file(c: &mut Ctx, src: &str, dst: &str, fail_if_exists: bool) -> bool {
    let (a, b) = (c.p.vpath(src), c.p.vpath(dst));
    if fail_if_exists && c.p.fs.exists(&b) {
        c.set_last_error(super::ERROR_ALREADY_EXISTS);
        return false;
    }
    let r = c.p.fs.is_file(&a) && c.p.fs.copy(&a, &b);
    if !r {
        c.set_last_error(ERROR_FILE_NOT_FOUND);
    }
    r
}

fn move_file(c: &mut Ctx, src: &str, dst: &str, replace: bool) -> bool {
    let (a, b) = (c.p.vpath(src), c.p.vpath(dst));
    if c.p.fs.exists(&b) {
        if !replace {
            c.set_last_error(super::ERROR_ALREADY_EXISTS);
            return false;
        }
        c.p.fs.remove(&b);
    }
    let r = c.p.fs.rename(&a, &b);
    if !r {
        c.set_last_error(ERROR_FILE_NOT_FOUND);
    }
    r
}

fn delete_path(c: &mut Ctx, guest: &str, dir: bool) -> bool {
    let key = c.p.vpath(guest);
    let ok_ = if dir { c.p.fs.is_dir(&key) } else { c.p.fs.is_file(&key) };
    let r = ok_ && c.p.fs.remove(&key);
    if !r {
        c.set_last_error(if ok_ { 145 } else { ERROR_FILE_NOT_FOUND });
    }
    r
}

fn create_dir(c: &mut Ctx, guest: &str) -> bool {
    let key = c.p.vpath(guest);
    if c.p.fs.exists(&key) {
        c.set_last_error(super::ERROR_ALREADY_EXISTS);
        return false;
    }
    let r = c.p.fs.mkdir(&key);
    if !r {
        c.set_last_error(super::ERROR_PATH_NOT_FOUND);
    }
    r
}

pub fn register_more(r: &mut Registry) {
    let k = "kernel32.dll";
    // ---- strings ----------------------------------------------------------------
    r.add(k, "lstrcpyW", |c| {
        let (dst, src) = (c.arg(0), c.arg(1));
        let s = wstr(c, src);
        let _ = c.host.write_wstr(dst, &s);
        ok(2, dst)
    });
    r.add(k, "lstrcpynW", |c| {
        let (dst, src, n) = (c.arg(0), c.arg(1), c.arg(2));
        let s = wstr(c, src);
        copy_wstr_out(c, dst, n, &s);
        ok(3, dst)
    });
    r.add(k, "lstrcatW", |c| {
        let (dst, src) = (c.arg(0), c.arg(1));
        let s = format!("{}{}", wstr(c, dst), wstr(c, src));
        let _ = c.host.write_wstr(dst, &s);
        ok(2, dst)
    });
    r.add(k, "GetStringTypeExA", |c| {
        let (src, n, out) = (c.arg(2), c.arg(3) as i32, c.arg(4));
        let bytes = read_sized(c, src, n);
        for (i, b) in bytes.iter().enumerate() {
            let _ = c.host.write_u16(out + 2 * i as u32, ctype1(*b));
        }
        ok(5, 1)
    });
    r.add(k, "GetStringTypeExW", |c| {
        let (src, n, out) = (c.arg(2), c.arg(3) as i32, c.arg(4));
        let count = if n < 0 { c.host.read_wstr(src).map(|s| s.encode_utf16().count()).unwrap_or(0) } else { n as usize };
        for i in 0..count {
            let u = c.host.read_u16(src + 2 * i as u32).unwrap_or(0);
            let _ = c.host.write_u16(out + 2 * i as u32, if u < 256 { ctype1(u as u8) } else { 0x100 });
        }
        ok(5, 1)
    });
    r.add(k, "ExpandEnvironmentStringsW", |c| {
        let s = wstr(c, c.arg(0));
        let e = expand_env(c, &s);
        let n = copy_wstr_out(c, c.arg(1), c.arg(2), &e);
        ok(3, n + 1)
    });
    r.add(k, "ExpandEnvironmentStringsA", |c| {
        let s = c.str_arg(0);
        let e = expand_env(c, &s);
        let n = copy_str_out(c, c.arg(1), c.arg(2), e.as_bytes());
        ok(3, n + 1)
    });
    r.add(k, "FormatMessageW", |c| format_message(c, true));
    r.add(k, "FormatMessageA", |c| format_message(c, false));
    r.add(k, "GetDateFormatW", |c| date_time_format(c, true, false));
    r.add(k, "GetDateFormatA", |c| date_time_format(c, false, false));
    r.add(k, "GetTimeFormatW", |c| date_time_format(c, true, true));
    r.add(k, "GetTimeFormatA", |c| date_time_format(c, false, true));
    r.alias(k, "GetDateFormatEx", k, "GetDateFormatW");
    r.alias(k, "GetTimeFormatEx", k, "GetTimeFormatW");
    r.add(k, "MulDiv", |c| {
        let (a, b, d) = (c.arg_i32(0) as i64, c.arg_i32(1) as i64, c.arg_i32(2) as i64);
        if d == 0 {
            return ok(3, 0xFFFF_FFFF);
        }
        let prod = a * b;
        let r = if (prod < 0) == (d < 0) { (prod + d / 2) / d } else { (prod - d / 2) / d };
        ok(3, r as i32 as u32)
    });
    r.add(k, "VerLanguageNameW", |c| {
        let n = copy_wstr_out(c, c.arg(1), c.arg(2), "English (United States)");
        ok(3, n)
    });
    r.add(k, "VerLanguageNameA", |c| {
        let n = copy_str_out(c, c.arg(1), c.arg(2), b"English (United States)");
        ok(3, n)
    });
    r.add(k, "EnumCalendarInfoW", |_c| ok(4, 1));
    r.add(k, "EnumCalendarInfoA", |_c| ok(4, 1));
    r.add(k, "EnumCalendarInfoExW", |_c| ok(4, 1));
    r.add(k, "EnumCalendarInfoExA", |_c| ok(4, 1));
    r.add(k, "EnumCalendarInfoExEx", |_c| ok(5, 1));

    // ---- system information ------------------------------------------------------
    r.add(k, "GetComputerNameW", |c| {
        let (buf, psize) = (c.arg(0), c.arg(1));
        let cap = c.host.read_u32(psize).unwrap_or(0);
        let n = copy_wstr_out(c, buf, cap, "GMRECOMP");
        let _ = c.host.write_u32(psize, n);
        ok(2, (cap > 8) as u32)
    });
    r.add(k, "GetComputerNameA", |c| {
        let (buf, psize) = (c.arg(0), c.arg(1));
        let cap = c.host.read_u32(psize).unwrap_or(0);
        let n = copy_str_out(c, buf, cap, b"GMRECOMP");
        let _ = c.host.write_u32(psize, n);
        ok(2, (cap > 8) as u32)
    });
    r.alias(k, "GetComputerNameExW", k, "GetComputerNameW");
    r.alias(k, "GetComputerNameExA", k, "GetComputerNameA");
    r.add(k, "GetVolumeInformationW", |c| {
        copy_wstr_out(c, c.arg(1), c.arg(2), "GMRECOMP");
        write_out(c, c.arg(3), 0x1234_5678);
        write_out(c, c.arg(4), 255);
        write_out(c, c.arg(5), 0x0007_00FF);
        copy_wstr_out(c, c.arg(6), c.arg(7), "NTFS");
        ok(8, 1)
    });
    r.add(k, "GetVolumeInformationA", |c| {
        copy_str_out(c, c.arg(1), c.arg(2), b"GMRECOMP");
        write_out(c, c.arg(3), 0x1234_5678);
        write_out(c, c.arg(4), 255);
        write_out(c, c.arg(5), 0x0007_00FF);
        copy_str_out(c, c.arg(6), c.arg(7), b"NTFS");
        ok(8, 1)
    });
    r.add(k, "GetDiskFreeSpaceW", |c| {
        write_out(c, c.arg(1), 8);
        write_out(c, c.arg(2), 512);
        write_out(c, c.arg(3), 0x0010_0000);
        write_out(c, c.arg(4), 0x0040_0000);
        ok(5, 1)
    });
    r.alias(k, "GetDiskFreeSpaceA", k, "GetDiskFreeSpaceW");
    r.add(k, "GetDiskFreeSpaceExW", |c| {
        for i in 1..4 {
            let p = c.arg(i);
            if p != 0 {
                let _ = c.host.write_u64(p, 10u64 << 30);
            }
        }
        ok(4, 1)
    });
    r.alias(k, "GetDiskFreeSpaceExA", k, "GetDiskFreeSpaceExW");
    r.add(k, "GetDriveTypeW", |_c| ok(1, 3));
    r.add(k, "GetDriveTypeA", |_c| ok(1, 3));

    // ---- files and directories ---------------------------------------------------
    r.add(k, "CopyFileW", |c| {
        let (src, dst, fail) = (wstr(c, c.arg(0)), wstr(c, c.arg(1)), c.arg(2));
        let r = copy_file(c, &src, &dst, fail != 0);
        c.trace(format!("({src:?} -> {dst:?}) = {r}"));
        ok(3, r as u32)
    });
    r.add(k, "CopyFileA", |c| {
        let (src, dst, fail) = (c.str_arg(0), c.str_arg(1), c.arg(2));
        let r = copy_file(c, &src, &dst, fail != 0);
        ok(3, r as u32)
    });
    r.add(k, "MoveFileW", |c| {
        let (src, dst) = (wstr(c, c.arg(0)), wstr(c, c.arg(1)));
        let r = move_file(c, &src, &dst, false);
        ok(2, r as u32)
    });
    r.add(k, "MoveFileA", |c| {
        let (src, dst) = (c.str_arg(0), c.str_arg(1));
        let r = move_file(c, &src, &dst, false);
        ok(2, r as u32)
    });
    r.add(k, "MoveFileExW", |c| {
        let (src, dst, flags) = (wstr(c, c.arg(0)), wstr(c, c.arg(1)), c.arg(2));
        let r = move_file(c, &src, &dst, flags & 1 != 0);
        ok(3, r as u32)
    });
    r.add(k, "MoveFileExA", |c| {
        let (src, dst, flags) = (c.str_arg(0), c.str_arg(1), c.arg(2));
        let r = move_file(c, &src, &dst, flags & 1 != 0);
        ok(3, r as u32)
    });
    r.add(k, "DeleteFileW", |c| {
        let p = wstr(c, c.arg(0));
        let r = delete_path(c, &p, false);
        c.trace(format!("({p:?}) = {r}"));
        ok(1, r as u32)
    });
    r.add(k, "DeleteFileA", |c| {
        let p = c.str_arg(0);
        let r = delete_path(c, &p, false);
        ok(1, r as u32)
    });
    r.add(k, "CreateDirectoryW", |c| {
        let p = wstr(c, c.arg(0));
        let r = create_dir(c, &p);
        c.trace(format!("({p:?}) = {r}"));
        ok(2, r as u32)
    });
    r.add(k, "CreateDirectoryA", |c| {
        let p = c.str_arg(0);
        let r = create_dir(c, &p);
        ok(2, r as u32)
    });
    r.add(k, "RemoveDirectoryW", |c| {
        let p = wstr(c, c.arg(0));
        let r = delete_path(c, &p, true);
        ok(1, r as u32)
    });
    r.add(k, "RemoveDirectoryA", |c| {
        let p = c.str_arg(0);
        let r = delete_path(c, &p, true);
        ok(1, r as u32)
    });
    r.add(k, "SetFileAttributesW", |_c| ok(2, 1));
    r.add(k, "SetFileAttributesA", |_c| ok(2, 1));
    r.add(k, "GetFileAttributesExW", |c| {
        let p = wstr(c, c.arg(0));
        let out = c.arg(2);
        match c.p.fs.stat(&c.p.vpath(&p)) {
            Some(md) => {
                write_attr_data(c, out, &md);
                ok(3, 1)
            }
            None => {
                c.set_last_error(ERROR_FILE_NOT_FOUND);
                ok(3, 0)
            }
        }
    });
    r.add(k, "GetFileAttributesExA", |c| {
        let p = c.str_arg(0);
        let out = c.arg(2);
        match c.p.fs.stat(&c.p.vpath(&p)) {
            Some(md) => {
                write_attr_data(c, out, &md);
                ok(3, 1)
            }
            None => {
                c.set_last_error(ERROR_FILE_NOT_FOUND);
                ok(3, 0)
            }
        }
    });
    r.add(k, "GetFullPathNameW", |c| {
        let name = wstr(c, c.arg(0));
        let (cch, buf, part) = (c.arg(1), c.arg(2), c.arg(3));
        let full = full_path(c, &name);
        let units = full.encode_utf16().count() as u32;
        if cch <= units {
            return ok(4, units + 1);
        }
        copy_wstr_out(c, buf, cch, &full);
        if part != 0 {
            let off = full.rfind('\\').map(|i| full[..i + 1].encode_utf16().count() as u32).unwrap_or(0);
            let _ = c.host.write_u32(part, if full.ends_with('\\') { 0 } else { buf + 2 * off });
        }
        ok(4, units)
    });
    r.add(k, "GetFullPathNameA", |c| {
        let name = c.str_arg(0);
        let (cch, buf, part) = (c.arg(1), c.arg(2), c.arg(3));
        let full = full_path(c, &name);
        if cch as usize <= full.len() {
            return ok(4, full.len() as u32 + 1);
        }
        copy_str_out(c, buf, cch, full.as_bytes());
        if part != 0 {
            let off = full.rfind('\\').map(|i| i as u32 + 1).unwrap_or(0);
            let _ = c.host.write_u32(part, if full.ends_with('\\') { 0 } else { buf + off });
        }
        ok(4, full.len() as u32)
    });
    r.add(k, "SearchPathW", |c| {
        let file = wstr(c, c.arg(1));
        let (cch, buf, part) = (c.arg(3), c.arg(4), c.arg(5));
        if !c.p.fs.is_file(&c.p.vpath(&file)) {
            c.set_last_error(ERROR_FILE_NOT_FOUND);
            return ok(6, 0);
        }
        let full = full_path(c, &file);
        let units = full.encode_utf16().count() as u32;
        if cch <= units {
            return ok(6, units + 1);
        }
        copy_wstr_out(c, buf, cch, &full);
        if part != 0 {
            let off = full.rfind('\\').map(|i| full[..i + 1].encode_utf16().count() as u32).unwrap_or(0);
            let _ = c.host.write_u32(part, buf + 2 * off);
        }
        ok(6, units)
    });
    r.add(k, "SearchPathA", |c| {
        let file = c.str_arg(1);
        let (cch, buf, part) = (c.arg(3), c.arg(4), c.arg(5));
        if !c.p.fs.is_file(&c.p.vpath(&file)) {
            c.set_last_error(ERROR_FILE_NOT_FOUND);
            return ok(6, 0);
        }
        let full = full_path(c, &file);
        if cch as usize <= full.len() {
            return ok(6, full.len() as u32 + 1);
        }
        copy_str_out(c, buf, cch, full.as_bytes());
        if part != 0 {
            let off = full.rfind('\\').map(|i| i as u32 + 1).unwrap_or(0);
            let _ = c.host.write_u32(part, buf + off);
        }
        ok(6, full.len() as u32)
    });
    r.add(k, "SetCurrentDirectoryW", |c| {
        let p = wstr(c, c.arg(0));
        let full = full_path(c, &p);
        if !c.p.fs.is_dir(&c.p.vpath(&full)) {
            c.set_last_error(super::ERROR_PATH_NOT_FOUND);
            return ok(1, 0);
        }
        c.trace(format!("({full:?})"));
        c.p.cwd = full.trim_end_matches('\\').to_string();
        ok(1, 1)
    });
    r.add(k, "SetCurrentDirectoryA", |c| {
        let p = c.str_arg(0);
        let full = full_path(c, &p);
        if !c.p.fs.is_dir(&c.p.vpath(&full)) {
            c.set_last_error(super::ERROR_PATH_NOT_FOUND);
            return ok(1, 0);
        }
        c.p.cwd = full.trim_end_matches('\\').to_string();
        ok(1, 1)
    });
    r.add(k, "FindFirstFileW", |c| {
        let pat = wstr(c, c.arg(0));
        let out = c.arg(1);
        let h = find_first(c, &pat);
        if h == INVALID_HANDLE_VALUE {
            c.set_last_error(ERROR_FILE_NOT_FOUND);
        } else {
            find_next_w(c, h, out);
        }
        c.trace(format!("({pat:?}) = {h:#x}"));
        ok(2, h)
    });
    r.add(k, "FindFirstFileExW", |c| {
        let pat = wstr(c, c.arg(0));
        let out = c.arg(2);
        let h = find_first(c, &pat);
        if h == INVALID_HANDLE_VALUE {
            c.set_last_error(ERROR_FILE_NOT_FOUND);
        } else {
            find_next_w(c, h, out);
        }
        ok(6, h)
    });
    r.add(k, "FindNextFileW", |c| {
        let (h, out) = (c.arg(0), c.arg(1));
        let r = find_next_w(c, h, out);
        if !r {
            c.set_last_error(ERROR_NO_MORE_FILES);
        }
        ok(2, r as u32)
    });

    // ---- resources ----------------------------------------------------------------
    r.add(k, "FindResourceW", |c| find_resource_api(c, true, false));
    r.add(k, "FindResourceA", |c| find_resource_api(c, false, false));
    r.add(k, "FindResourceExW", |c| find_resource_api(c, true, true));
    r.add(k, "FindResourceExA", |c| find_resource_api(c, false, true));
    r.add(k, "LoadResource", |c| {
        let h = c.arg(1);
        let v = match c.p.handles.get(&h) {
            Some(Handle::Resource { addr, .. }) => *addr,
            _ => 0,
        };
        ok(2, v)
    });
    r.add(k, "LockResource", |c| ok(1, c.arg(0)));
    r.add(k, "SizeofResource", |c| {
        let h = c.arg(1);
        let v = match c.p.handles.get(&h) {
            Some(Handle::Resource { size, .. }) => *size,
            _ => 0,
        };
        ok(2, v)
    });
    r.add(k, "FreeResource", |_c| ok(1, 0));
    r.add(k, "EnumResourceNamesW", |_c| ok(4, 0));
    r.add(k, "EnumResourceNamesA", |_c| ok(4, 0));

    // ---- atoms ----------------------------------------------------------------------
    r.add(k, "GlobalAddAtomW", |c| {
        let a = c.arg(0);
        let name = if a < 0x10000 { return ok(1, a) } else { wstr(c, a) };
        let v = global_atom(c, &name);
        ok(1, v as u32)
    });
    r.add(k, "GlobalAddAtomA", |c| {
        let a = c.arg(0);
        let name = if a < 0x10000 { return ok(1, a) } else { c.str_arg(0) };
        let v = global_atom(c, &name);
        ok(1, v as u32)
    });
    r.add(k, "GlobalFindAtomW", |c| {
        let name = wstr(c, c.arg(0));
        ok(1, c.p.atoms.get(&name.to_lowercase()).copied().unwrap_or(0) as u32)
    });
    r.add(k, "GlobalFindAtomA", |c| {
        let name = c.str_arg(0);
        ok(1, c.p.atoms.get(&name.to_lowercase()).copied().unwrap_or(0) as u32)
    });
    r.add(k, "GlobalDeleteAtom", |_c| ok(1, 0));
    r.add(k, "GlobalGetAtomNameW", |c| {
        let a = c.arg(0) as u16;
        let name = c.p.atoms.iter().find(|(_, v)| **v == a).map(|(n, _)| n.clone()).unwrap_or_default();
        let n = copy_wstr_out(c, c.arg(1), c.arg(2), &name);
        ok(3, n)
    });
    r.add(k, "GlobalGetAtomNameA", |c| {
        let a = c.arg(0) as u16;
        let name = c.p.atoms.iter().find(|(_, v)| **v == a).map(|(n, _)| n.clone()).unwrap_or_default();
        let n = copy_str_out(c, c.arg(1), c.arg(2), name.as_bytes());
        ok(3, n)
    });
    r.alias(k, "AddAtomW", k, "GlobalAddAtomW");
    r.alias(k, "AddAtomA", k, "GlobalAddAtomA");
    r.alias(k, "FindAtomW", k, "GlobalFindAtomW");
    r.alias(k, "FindAtomA", k, "GlobalFindAtomA");
    r.alias(k, "DeleteAtom", k, "GlobalDeleteAtom");

    // ---- mappings, processes, threads ----------------------------------------------
    r.add(k, "CreateFileMappingW", |c| {
        let (hfile, sizelo) = (c.arg(0), c.arg(4));
        let size = if hfile == INVALID_HANDLE_VALUE { sizelo } else { file_size(c, hfile).unwrap_or(0) as u32 };
        let h = c.p.new_handle(Handle::Mapping { size, addr: 0, file: if hfile == INVALID_HANDLE_VALUE { 0 } else { hfile } });
        c.trace(format!("({size} bytes) = {h:#x}"));
        ok(6, h)
    });
    r.alias(k, "CreateFileMappingA", k, "CreateFileMappingW");
    r.add(k, "OpenFileMappingW", |c| {
        c.set_last_error(ERROR_FILE_NOT_FOUND);
        ok(3, 0)
    });
    r.alias(k, "OpenFileMappingA", k, "OpenFileMappingW");
    r.add(k, "MapViewOfFile", |c| {
        let (h, offlo, size_req) = (c.arg(0), c.arg(3), c.arg(4));
        let (size, addr, file) = match c.p.handles.get(&h) {
            Some(Handle::Mapping { size, addr, file }) => (*size, *addr, *file),
            _ => {
                c.set_last_error(ERROR_INVALID_HANDLE);
                return ok(5, 0);
            }
        };
        let addr = if addr != 0 {
            addr
        } else {
            let total = size.max(size_req).max(1).div_ceil(0x1000) * 0x1000;
            let a = c.p.vm.alloc(0, total, true, PAGE_READWRITE, false, "file mapping").unwrap_or(0);
            if a == 0 || c.host.ensure_mapped(a, total).is_err() {
                return ok(5, 0);
            }
            let _ = c.host.fill(a, total, 0);
            if file != 0 {
                if let Some(pos) = seek_file(c, file, 0, 0) {
                    let _ = pos;
                    let mut off = 0;
                    while off < size {
                        match read_file(c, file, a + off, (size - off).min(1 << 20)) {
                            Some(n) if n > 0 => off += n,
                            _ => break,
                        }
                    }
                }
            }
            if let Some(Handle::Mapping { addr, .. }) = c.p.handles.get_mut(&h) {
                *addr = a;
            }
            a
        };
        ok(5, addr + offlo)
    });
    r.alias(k, "MapViewOfFileEx", k, "MapViewOfFile");
    r.add(k, "UnmapViewOfFile", |_c| ok(1, 1));
    r.add(k, "FlushViewOfFile", |_c| ok(2, 1));
    r.add(k, "OpenMutexW", |c| {
        c.set_last_error(ERROR_FILE_NOT_FOUND);
        ok(3, 0)
    });
    r.alias(k, "OpenMutexA", k, "OpenMutexW");
    r.add(k, "OpenEventW", |c| {
        c.set_last_error(ERROR_FILE_NOT_FOUND);
        ok(3, 0)
    });
    r.alias(k, "OpenEventA", k, "OpenEventW");
    r.add(k, "OpenProcess", |c| {
        c.set_last_error(super::ERROR_INVALID_PARAMETER);
        ok(3, 0)
    });
    r.add(k, "CreateProcessW", |c| {
        let app = wstr(c, c.arg(0));
        let cmd = wstr(c, c.arg(1));
        c.p.log(format!("CreateProcessW({app:?}, {cmd:?}) refused"));
        c.set_last_error(ERROR_FILE_NOT_FOUND);
        ok(10, 0)
    });
    r.add(k, "CreateProcessA", |c| {
        let app = c.str_arg(0);
        let cmd = c.str_arg(1);
        c.p.log(format!("CreateProcessA({app:?}, {cmd:?}) refused"));
        c.set_last_error(ERROR_FILE_NOT_FOUND);
        ok(10, 0)
    });
    r.add(k, "ExitThread", |c| {
        let code = c.arg(0);
        c.p.log(format!("ExitThread({code}) on the main thread: exiting"));
        c.host.request_exit(code);
        ok(1, 0)
    });
    r.add(k, "SignalObjectAndWait", |_c| ok(4, 0));
    r.add(k, "WaitForMultipleObjectsEx", |c| {
        let ms = c.arg(3);
        if ms != 0xFFFF_FFFF {
            c.advance_clock(ms as u64 * 1000);
        }
        ok(5, 0)
    });
    r.add(k, "VirtualQueryEx", |c| {
        let (addr, buf, len) = (c.arg(1), c.arg(2), c.arg(3));
        let (base, size, state, protect, alloc_base) = c.p.vm.query(addr);
        if len >= 28 {
            let _ = c.host.write_u32(buf, base);
            let _ = c.host.write_u32(buf + 4, alloc_base);
            let _ = c.host.write_u32(buf + 8, protect);
            let _ = c.host.write_u32(buf + 12, size);
            let _ = c.host.write_u32(buf + 16, state);
            let _ = c.host.write_u32(buf + 20, protect);
            let _ = c.host.write_u32(buf + 24, if state == crate::mem::MEM_FREE { 0 } else { crate::mem::MEM_PRIVATE });
        }
        ok(4, 28)
    });
}

fn global_atom(c: &mut Ctx, name: &str) -> u16 {
    let key = name.to_lowercase();
    if let Some(&a) = c.p.atoms.get(&key) {
        return a;
    }
    let a = c.p.next_atom;
    c.p.next_atom = c.p.next_atom.wrapping_add(1).max(0xC000);
    c.p.atoms.insert(key, a);
    a
}

pub fn register_paths(r: &mut Registry) {
    let k = "kernel32.dll";
    // Paths are already long; the 8.3 form is the same string.
    r.add(k, "GetLongPathNameW", |c| {
        let s = wstr(c, c.arg(0));
        let (buf, cch) = (c.arg(1), c.arg(2));
        let units = s.encode_utf16().count() as u32;
        if cch <= units {
            return ok(3, units + 1);
        }
        copy_wstr_out(c, buf, cch, &s);
        ok(3, units)
    });
    r.alias(k, "GetShortPathNameW", k, "GetLongPathNameW");
    r.add(k, "GetLongPathNameA", |c| {
        let s = c.str_arg(0);
        let (buf, cch) = (c.arg(1), c.arg(2));
        if cch as usize <= s.len() {
            return ok(3, s.len() as u32 + 1);
        }
        copy_str_out(c, buf, cch, s.as_bytes());
        ok(3, s.len() as u32)
    });
    r.alias(k, "GetShortPathNameA", k, "GetLongPathNameA");
}

/// Newer and rarer kernel32 entry points (GameMaker: Studio runners, their CRTs).
pub fn register_extra(r: &mut Registry) {
    let k = "kernel32.dll";
    r.add(k, "DisableThreadLibraryCalls", |_c| ok(1, 1));
    // Not a packaged (Store) app.
    r.add(k, "GetCurrentPackageId", |_c| ok(2, 15700));
    r.add(k, "GetCurrentPackageFullName", |_c| ok(2, 15700));
    r.add(k, "GetConsoleWindow", |_c| ok(0, 0));
    r.add(k, "IsDBCSLeadByte", |_c| ok(1, 0));
    r.add(k, "IsDBCSLeadByteEx", |_c| ok(2, 0));
    r.add(k, "AreFileApisANSI", |_c| ok(0, 1));
    r.add(k, "PeekNamedPipe", |c| {
        c.set_last_error(6); // ERROR_INVALID_HANDLE
        ok(6, 0)
    });
    r.add(k, "RtlCaptureStackBackTrace", |_c| ok(4, 0));
    r.add(k, "SetEnvironmentVariableW", |c| {
        let (n, v) = (wstr(c, c.arg(0)), wstr(c, c.arg(1)));
        c.p.env.retain(|(k2, _)| !k2.eq_ignore_ascii_case(&n));
        if c.arg(1) != 0 {
            c.p.env.push((n, v));
        }
        ok(2, 1)
    });
    r.add(k, "WriteConsoleW", |c| {
        let (buf, n, written) = (c.arg(1), c.arg(2), c.arg(3));
        let mut units = vec![0u16; n as usize];
        for (i, u) in units.iter_mut().enumerate() {
            *u = c.host.read_u16(buf + 2 * i as u32).unwrap_or(0);
        }
        c.p.log(format!("console: {}", String::from_utf16_lossy(&units).trim_end()));
        write_out(c, written, n);
        ok(5, 1)
    });
    r.add(k, "WriteConsoleA", |c| {
        let (buf, n, written) = (c.arg(1), c.arg(2), c.arg(3));
        let mut b = vec![0u8; n as usize];
        let _ = c.host.read(buf, &mut b);
        c.p.log(format!("console: {}", String::from_utf8_lossy(&b).trim_end()));
        write_out(c, written, n);
        ok(5, 1)
    });
    r.add(k, "ReadConsoleW", |c| {
        write_out(c, c.arg(3), 0);
        ok(5, 0)
    });
    r.add(k, "FatalAppExitA", |c| {
        let msg = c.str_arg(1);
        Err(ApiError(format!("FatalAppExit: {msg}")))
    });
    r.add(k, "FatalAppExitW", |c| {
        let msg = wstr(c, c.arg(1));
        Err(ApiError(format!("FatalAppExit: {msg}")))
    });
    r.add(k, "GetTempFileNameA", |c| {
        let (dir, prefix, unique, out) = (c.str_arg(0), c.str_arg(1), c.arg(2), c.arg(3));
        let (name, n) = temp_file_name(c, &dir, &prefix, unique);
        copy_str_out(c, out, 260, name.as_bytes());
        ok(4, n)
    });
    r.add(k, "GetTempFileNameW", |c| {
        let (dir, prefix, unique, out) = (wstr(c, c.arg(0)), wstr(c, c.arg(1)), c.arg(2), c.arg(3));
        let (name, n) = temp_file_name(c, &dir, &prefix, unique);
        copy_wstr_out(c, out, 260, &name);
        ok(4, n)
    });
    r.add(k, "EnumSystemLocalesA", |_c| ok(2, 1));
    r.add(k, "EnumSystemLocalesW", |_c| ok(2, 1));
    r.add(k, "EnumSystemLocalesEx", |_c| ok(4, 1));
    r.add(k, "IsValidLocaleName", |_c| ok(1, 1));
    r.add(k, "GetUserDefaultLocaleName", |c| {
        let n = copy_wstr_out(c, c.arg(0), c.arg(1), "en-US");
        ok(2, n + 1)
    });
    r.add(k, "GetSystemDefaultLocaleName", |c| {
        let n = copy_wstr_out(c, c.arg(0), c.arg(1), "en-US");
        ok(2, n + 1)
    });
    r.add(k, "LocaleNameToLCID", |_c| ok(2, 0x409));
    r.add(k, "LCIDToLocaleName", |c| {
        let n = copy_wstr_out(c, c.arg(1), c.arg(2), "en-US");
        ok(4, n + 1)
    });
    // The ...Ex forms take a locale name where the old ones take an LCID.
    r.alias_n(k, "CompareStringEx", k, "CompareStringW", 9);
    r.alias_n(k, "LCMapStringEx", k, "LCMapStringW", 9);
    r.alias_n(k, "GetLocaleInfoEx", k, "GetLocaleInfoW", 4);
    r.add(k, "FindFirstFileExA", |c| {
        let pattern = c.str_arg(0);
        let out = c.arg(2);
        let h = find_first(c, &pattern);
        if h == INVALID_HANDLE_VALUE || !find_next(c, h, out) {
            c.p.handles.remove(&h);
            c.set_last_error(ERROR_FILE_NOT_FOUND);
            return ok(6, INVALID_HANDLE_VALUE);
        }
        ok(6, h)
    });
    r.add(k, "InitOnceExecuteOnce", |c| {
        let (once, f, param, ctx) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        if c.host.read_u32(once).unwrap_or(0) & 2 == 0 {
            let r = c.call_guest(f, &[once, param, ctx])?;
            if r == 0 {
                return ok(4, 0);
            }
            let _ = c.host.write_u32(once, 2);
        }
        ok(4, 1)
    });
    r.add(k, "SystemTimeToTzSpecificLocalTime", |c| {
        let (utc, local) = (c.arg(1), c.arg(2));
        let mut b = [0u8; 16];
        let _ = c.host.read(utc, &mut b);
        let _ = c.host.write(local, &b);
        ok(3, 1)
    });
    r.add(k, "TzSpecificLocalTimeToSystemTime", |c| {
        let (local, utc) = (c.arg(1), c.arg(2));
        let mut b = [0u8; 16];
        let _ = c.host.read(local, &mut b);
        let _ = c.host.write(utc, &b);
        ok(3, 1)
    });
    // Waitable timers: signaled once the virtual clock passes the due time.
    r.add(k, "CreateWaitableTimerW", |c| {
        let manual = c.arg(1) != 0;
        let h = c.p.new_handle(Handle::Event { manual, signaled: false });
        ok(3, h)
    });
    r.alias(k, "CreateWaitableTimerA", k, "CreateWaitableTimerW");
    r.add(k, "SetWaitableTimer", |c| {
        let (h, due_p) = (c.arg(0), c.arg(1));
        let due = c.host.read_u64(due_p).unwrap_or(0) as i64;
        // negative: relative, in 100 ns units; positive: absolute FILETIME (taken as now)
        let delay_us = if due < 0 { (-due) as u64 / 10 } else { 0 };
        let at = c.p.clock_us + delay_us;
        c.p.timers_due.push((h, at));
        if let Some(Handle::Event { signaled, .. }) = c.p.handles.get_mut(&h) {
            *signaled = delay_us == 0;
        }
        ok(6, 1)
    });
    r.add(k, "CancelWaitableTimer", |c| {
        let h = c.arg(0);
        c.p.timers_due.retain(|&(t, _)| t != h);
        ok(1, 1)
    });
}

/// GetTempFileName: `dir\pre<hex>.tmp`; with unique = 0, a new number whose file is created.
fn temp_file_name(c: &mut Ctx, dir: &str, prefix: &str, unique: u32) -> (String, u32) {
    let pre: String = prefix.chars().take(3).collect();
    let dir = dir.trim_end_matches('\\');
    if unique != 0 {
        return (format!("{dir}\\{pre}{:X}.tmp", unique & 0xFFFF), unique & 0xFFFF);
    }
    let mut n = (c.p.clock_us as u32 & 0xFFFF).max(1);
    loop {
        let name = format!("{dir}\\{pre}{n:X}.tmp");
        let key = c.p.vpath(&name);
        if !c.p.fs.exists(&key) {
            c.p.fs.create(&key);
            return (name, n);
        }
        n = (n % 0xFFFF) + 1;
    }
}
