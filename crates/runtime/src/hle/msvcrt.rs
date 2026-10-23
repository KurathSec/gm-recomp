//! msvcrt.dll (the system C runtime that game DLLs such as supersound.dll link
//! against). Everything is cdecl. Floating-point results come back in ST(0) and the
//! `_CI*` intrinsics take their operands from the x87 stack, so these go through the
//! host's FPU.

use super::{ApiError, ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use crate::process::Handle;

fn ok(v: u32) -> ApiResult {
    Ok(Ret::cdecl(v))
}

/// Returns a double in ST(0).
fn ret_f64(c: &mut Ctx, v: f64) -> ApiResult {
    c.host.fpu_push(v);
    ok(0)
}

fn arg_f64(c: &Ctx, i: u32) -> f64 {
    let lo = c.arg(i) as u64;
    let hi = c.arg(i + 1) as u64;
    f64::from_bits(lo | (hi << 32))
}

fn alloc(c: &mut Ctx, size: u32, zero: bool) -> u32 {
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    c.p.heap_alloc(g, heap, size, zero)
}

fn errno_addr(c: &mut Ctx) -> u32 {
    if c.p.crt_errno == 0 {
        c.p.crt_errno = alloc(c, 4, true);
    }
    c.p.crt_errno
}

fn set_errno(c: &mut Ctx, e: u32) {
    let a = errno_addr(c);
    let _ = c.host.write_u32(a, e);
}

fn read_bytes(c: &Ctx, p: u32, n: u32) -> Vec<u8> {
    let mut v = vec![0u8; n as usize];
    let _ = c.host.read(p, &mut v);
    v
}

// ---- printf -------------------------------------------------------------------------

/// Reads successive cdecl variadic arguments: 4-byte slots (8-byte ones for 64-bit
/// values), or 8-byte slots for everything in a 64-bit process.
pub struct VaList {
    pub p: u32,
    pub x64: bool,
}

impl VaList {
    /// A `va_list` (or the variadic part of the stack) at `p`.
    pub fn new(c: &Ctx, p: u32) -> Self {
        VaList { p, x64: c.x64() }
    }
    fn u32(&mut self, c: &Ctx) -> u32 {
        let v = c.host.read_u32(self.p).unwrap_or(0);
        self.p += if self.x64 { 8 } else { 4 };
        v
    }
    fn u64(&mut self, c: &Ctx) -> u64 {
        let v = c.host.read_u64(self.p).unwrap_or(0);
        self.p += 8;
        v
    }
}

/// MSVC-style exponent: at least three digits.
fn fmt_exp(v: f64, prec: usize, upper: bool) -> String {
    let s = format!("{v:.prec$e}");
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let e: i32 = exp.parse().unwrap_or(0);
    let sign = if e < 0 { '-' } else { '+' };
    let out = format!("{mant}e{sign}{:03}", e.abs());
    if upper { out.to_uppercase() } else { out }
}

fn strip_zeros(s: String) -> String {
    if let Some(epos) = s.find(['e', 'E']) {
        let (m, e) = s.split_at(epos);
        let m = if m.contains('.') { m.trim_end_matches('0').trim_end_matches('.') } else { m };
        format!("{m}{e}")
    } else if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

fn fmt_float(v: f64, conv: u8, prec: Option<usize>, alt: bool) -> String {
    if v.is_nan() {
        return "1.#QNAN0".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-1.#INF00".into() } else { "1.#INF00".into() };
    }
    match conv {
        b'f' | b'F' => format!("{v:.*}", prec.unwrap_or(6)),
        b'e' | b'E' => fmt_exp(v, prec.unwrap_or(6), conv == b'E'),
        _ => {
            let p = prec.unwrap_or(6).max(1);
            let x = if v == 0.0 {
                0
            } else {
                let s = format!("{:.*e}", p - 1, v);
                s.split_once('e').map(|(_, e)| e.parse::<i32>().unwrap_or(0)).unwrap_or(0)
            };
            let s = if x < -4 || x >= p as i32 { fmt_exp(v, p - 1, conv == b'G') } else { format!("{v:.*}", (p as i32 - 1 - x).max(0) as usize) };
            if alt { s } else { strip_zeros(s) }
        }
    }
}

/// C printf formatting over guest arguments; `wide` selects %s/%c semantics for
/// the wide-character variants.
pub fn format(c: &Ctx, fmt: &[u8], va: &mut VaList, wide: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        let ch = fmt[i];
        i += 1;
        if ch != b'%' {
            out.push(ch);
            continue;
        }
        let (mut left, mut plus, mut space, mut alt, mut zero) = (false, false, false, false, false);
        while i < fmt.len() {
            match fmt[i] {
                b'-' => left = true,
                b'+' => plus = true,
                b' ' => space = true,
                b'#' => alt = true,
                b'0' => zero = true,
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        if i < fmt.len() && fmt[i] == b'*' {
            let w = va.u32(c) as i32;
            if w < 0 {
                left = true;
            }
            width = w.unsigned_abs() as usize;
            i += 1;
        } else {
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                width = width * 10 + (fmt[i] - b'0') as usize;
                i += 1;
            }
        }
        let mut prec = None;
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            let mut p = 0usize;
            if i < fmt.len() && fmt[i] == b'*' {
                let v = va.u32(c) as i32;
                p = v.max(0) as usize;
                i += 1;
            } else {
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    p = p * 10 + (fmt[i] - b'0') as usize;
                    i += 1;
                }
            }
            prec = Some(p);
        }
        let mut long64 = false;
        let mut short = false;
        let mut widechar = wide;
        loop {
            if i >= fmt.len() {
                break;
            }
            match fmt[i] {
                b'l' => {
                    if i + 1 < fmt.len() && fmt[i + 1] == b'l' {
                        long64 = true;
                        i += 1;
                    } else {
                        widechar = true;
                    }
                }
                b'h' => {
                    short = true;
                    widechar = false;
                }
                b'w' => widechar = true,
                b'I' if fmt[i..].starts_with(b"I64") => {
                    long64 = true;
                    i += 2;
                }
                b'I' if fmt[i..].starts_with(b"I32") => i += 2,
                b'I' => {}
                b'L' | b'z' | b't' | b'j' => {}
                _ => break,
            }
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else { break };
        i += 1;
        let mut sign = String::new();
        let body: Vec<u8> = match conv {
            b'd' | b'i' => {
                let v: i64 = if long64 { va.u64(c) as i64 } else if short { va.u32(c) as i16 as i64 } else { va.u32(c) as i32 as i64 };
                if v < 0 {
                    sign.push('-');
                } else if plus {
                    sign.push('+');
                } else if space {
                    sign.push(' ');
                }
                let mut d = v.unsigned_abs().to_string();
                if let Some(p) = prec {
                    if d.len() < p {
                        d = "0".repeat(p - d.len()) + &d;
                    }
                    if p == 0 && v == 0 {
                        d.clear();
                    }
                }
                d.into_bytes()
            }
            b'u' | b'x' | b'X' | b'o' => {
                let v: u64 = if long64 { va.u64(c) } else if short { va.u32(c) as u16 as u64 } else { va.u32(c) as u64 };
                let mut d = match conv {
                    b'u' => v.to_string(),
                    b'x' => format!("{v:x}"),
                    b'X' => format!("{v:X}"),
                    _ => format!("{v:o}"),
                };
                if let Some(p) = prec {
                    if d.len() < p {
                        d = "0".repeat(p - d.len()) + &d;
                    }
                }
                if alt && v != 0 {
                    match conv {
                        b'x' => sign.push_str("0x"),
                        b'X' => sign.push_str("0X"),
                        b'o' => d.insert(0, '0'),
                        _ => {}
                    }
                }
                d.into_bytes()
            }
            b'p' => format!("{:08X}", va.u32(c)).into_bytes(),
            b'c' | b'C' => {
                let v = va.u32(c);
                if widechar ^ (conv == b'C') {
                    char::from_u32(v & 0xFFFF).unwrap_or('?').to_string().into_bytes()
                } else {
                    vec![v as u8]
                }
            }
            b's' | b'S' => {
                let p = va.u32(c);
                let w = widechar ^ (conv == b'S');
                let mut s: Vec<u8> = if p == 0 {
                    b"(null)".to_vec()
                } else if w {
                    c.host.read_wstr(p).unwrap_or_default().into_bytes()
                } else {
                    c.host.read_cstr(p).unwrap_or_default()
                };
                if let Some(pr) = prec {
                    s.truncate(pr);
                }
                s
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let v = f64::from_bits(va.u64(c));
                if v.is_sign_negative() && !v.is_nan() {
                    sign.push('-');
                } else if plus {
                    sign.push('+');
                } else if space {
                    sign.push(' ');
                }
                fmt_float(v.abs(), conv, prec, alt).into_bytes()
            }
            b'n' => {
                let p = va.u32(c);
                let _ = p;
                Vec::new()
            }
            b'%' => b"%".to_vec(),
            other => vec![b'%', other],
        };
        let len = sign.len() + body.len();
        let pad = width.saturating_sub(len);
        let numeric = matches!(conv, b'd' | b'i' | b'u' | b'x' | b'X' | b'o' | b'f' | b'F' | b'e' | b'E' | b'g' | b'G');
        if left {
            out.extend_from_slice(sign.as_bytes());
            out.extend_from_slice(&body);
            out.extend(std::iter::repeat_n(b' ', pad));
        } else if zero && numeric && prec.is_none_or(|_| matches!(conv, b'f' | b'F' | b'e' | b'E' | b'g' | b'G')) {
            out.extend_from_slice(sign.as_bytes());
            out.extend(std::iter::repeat_n(b'0', pad));
            out.extend_from_slice(&body);
        } else {
            out.extend(std::iter::repeat_n(b' ', pad));
            out.extend_from_slice(sign.as_bytes());
            out.extend_from_slice(&body);
        }
    }
    out
}

// ---- stdio ------------------------------------------------------------------------------

/// FILE* -> kernel32 file handle.
fn file_handle(c: &Ctx, f: u32) -> Option<u32> {
    c.p.crt_files.get(&f).copied()
}

fn fopen(c: &mut Ctx, path: &str, mode: &str) -> u32 {
    let (access, disp) = match mode.trim_end_matches(['b', 't']).chars().next() {
        Some('r') if mode.contains('+') => (0xC000_0000, 3),
        Some('r') => (0x8000_0000, 3),
        Some('w') => (0xC000_0000, 2),
        Some('a') => (0xC000_0000, 4),
        _ => (0x8000_0000, 3),
    };
    let h = super::kernel32::create_file(c, path, access, disp);
    if h == super::INVALID_HANDLE_VALUE {
        set_errno(c, 2);
        return 0;
    }
    if mode.starts_with('a') {
        super::kernel32::seek_file(c, h, 0, 2);
    }
    let f = alloc(c, 32, true);
    c.p.crt_files.insert(f, h);
    f
}

fn crt_exit(c: &mut Ctx) -> ApiResult {
    let code = c.arg(0);
    c.p.log(format!("msvcrt exit({code})"));
    c.host.request_exit(code);
    ok(0)
}

// ---- qsort (the MSVC CRT algorithm, so equal elements end up where the real one puts them)

fn qsort_cmp(c: &mut Ctx, f: u32, a: u32, b: u32) -> Result<i32, ApiError> {
    Ok(c.call_guest(f, &[a, b])? as i32)
}

fn qsort_swap(c: &mut Ctx, a: u32, b: u32, w: u32) {
    if a != b {
        let x = read_bytes(c, a, w);
        let y = read_bytes(c, b, w);
        let _ = c.host.write(a, &y);
        let _ = c.host.write(b, &x);
    }
}

fn shortsort(c: &mut Ctx, lo: u32, mut hi: u32, w: u32, f: u32) -> Result<(), ApiError> {
    while hi > lo {
        let mut max = lo;
        let mut p = lo + w;
        while p <= hi {
            if qsort_cmp(c, f, p, max)? > 0 {
                max = p;
            }
            p += w;
        }
        qsort_swap(c, max, hi, w);
        hi -= w;
    }
    Ok(())
}

fn qsort(c: &mut Ctx, base: u32, num: u32, w: u32, f: u32) -> Result<(), ApiError> {
    if num < 2 || w == 0 {
        return Ok(());
    }
    let mut stack: Vec<(u32, u32)> = Vec::new();
    let (mut lo, mut hi) = (base, base + w * (num - 1));
    loop {
        let size = (hi - lo) / w + 1;
        if size <= 8 {
            shortsort(c, lo, hi, w, f)?;
        } else {
            let mut mid = lo + (size / 2) * w;
            if qsort_cmp(c, f, lo, mid)? > 0 {
                qsort_swap(c, lo, mid, w);
            }
            if qsort_cmp(c, f, lo, hi)? > 0 {
                qsort_swap(c, lo, hi, w);
            }
            if qsort_cmp(c, f, mid, hi)? > 0 {
                qsort_swap(c, mid, hi, w);
            }
            let (mut loguy, mut higuy) = (lo, hi);
            loop {
                if mid > loguy {
                    loop {
                        loguy += w;
                        if !(loguy < mid && qsort_cmp(c, f, loguy, mid)? <= 0) {
                            break;
                        }
                    }
                }
                if mid <= loguy {
                    loop {
                        loguy += w;
                        if !(loguy <= hi && qsort_cmp(c, f, loguy, mid)? <= 0) {
                            break;
                        }
                    }
                }
                loop {
                    higuy -= w;
                    if !(higuy > mid && qsort_cmp(c, f, higuy, mid)? > 0) {
                        break;
                    }
                }
                if higuy < loguy {
                    break;
                }
                qsort_swap(c, loguy, higuy, w);
                if mid == higuy {
                    mid = loguy;
                }
            }
            higuy += w;
            if mid < higuy {
                loop {
                    higuy -= w;
                    if !(higuy > mid && qsort_cmp(c, f, higuy, mid)? == 0) {
                        break;
                    }
                }
            }
            if mid >= higuy {
                loop {
                    higuy -= w;
                    if !(higuy > lo && qsort_cmp(c, f, higuy, mid)? == 0) {
                        break;
                    }
                }
            }
            if higuy.wrapping_sub(lo) as i32 >= hi.wrapping_sub(loguy) as i32 {
                if lo < higuy {
                    stack.push((lo, higuy));
                }
                if loguy < hi {
                    lo = loguy;
                    continue;
                }
            } else {
                if loguy < hi {
                    stack.push((loguy, hi));
                }
                if lo < higuy {
                    hi = higuy;
                    continue;
                }
            }
        }
        match stack.pop() {
            Some((l, h)) => {
                lo = l;
                hi = h;
            }
            None => return Ok(()),
        }
    }
}

pub fn register(r: &mut Registry) {
    let m = "msvcrt.dll";
    // ---- memory -------------------------------------------------------------------
    r.add(m, "malloc", |c| {
        let n = c.arg(0);
        let p = alloc(c, n, false);
        ok(p)
    });
    r.add(m, "calloc", |c| {
        let n = c.arg(0).saturating_mul(c.arg(1));
        let p = alloc(c, n, true);
        ok(p)
    });
    r.add(m, "realloc", |c| {
        let (old, n) = (c.arg(0), c.arg(1));
        if old == 0 {
            let p = alloc(c, n, false);
            return ok(p);
        }
        if n == 0 {
            c.p.heap_free(old);
            return ok(0);
        }
        let p = alloc(c, n, false);
        let copy = c.p.heap_size(old).unwrap_or(0).min(n);
        let data = read_bytes(c, old, copy);
        let _ = c.host.write(p, &data);
        c.p.heap_free(old);
        ok(p)
    });
    r.add(m, "free", |c| {
        let p = c.arg(0);
        if p != 0 {
            c.p.heap_free(p);
        }
        ok(0)
    });
    r.add(m, "_msize", |c| {
        let p = c.arg(0);
        ok(c.p.heap_size(p).unwrap_or(0xFFFF_FFFF))
    });
    r.alias(m, "??2@YAPAXI@Z", m, "malloc");
    r.alias(m, "??3@YAXPAX@Z", m, "free");
    r.alias(m, "??_U@YAPAXI@Z", m, "malloc");
    r.alias(m, "??_V@YAXPAX@Z", m, "free");
    r.add(m, "memcpy", |c| {
        let (d, s, n) = (c.arg(0), c.arg(1), c.arg(2));
        if c.host.copy_within(d, s, n).is_err() {
            // (a range that faults: what can be read, as before)
            let data = read_bytes(c, s, n);
            let _ = c.host.write(d, &data);
        }
        ok(d)
    });
    r.alias(m, "memmove", m, "memcpy");
    r.add(m, "memset", |c| {
        let (d, v, n) = (c.arg(0), c.arg(1), c.arg(2));
        let _ = c.host.fill(d, n, v as u8);
        ok(d)
    });
    r.add(m, "memcmp", |c| {
        let (a, b, n) = (c.arg(0), c.arg(1), c.arg(2));
        let (x, y) = (read_bytes(c, a, n), read_bytes(c, b, n));
        ok(match x.cmp(&y) {
            std::cmp::Ordering::Less => -1i32 as u32,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })
    });
    r.add(m, "memchr", |c| {
        let (p, v, n) = (c.arg(0), c.arg(1) as u8, c.arg(2));
        let data = read_bytes(c, p, n);
        ok(data.iter().position(|&b| b == v).map(|i| p + i as u32).unwrap_or(0))
    });

    // ---- strings ------------------------------------------------------------------
    r.add(m, "strlen", |c| {
        let p = c.arg(0);
        ok(c.host.read_cstr(p).map(|s| s.len() as u32).unwrap_or(0))
    });
    r.add(m, "strcpy", |c| {
        let (d, s) = (c.arg(0), c.arg(1));
        let v = c.host.read_cstr(s).unwrap_or_default();
        let _ = c.host.write_cstr(d, &v);
        ok(d)
    });
    r.add(m, "strncpy", |c| {
        let (d, s, n) = (c.arg(0), c.arg(1), c.arg(2));
        let mut v = c.host.read_cstr(s).unwrap_or_default();
        v.truncate(n as usize);
        v.resize(n as usize, 0);
        let _ = c.host.write(d, &v);
        ok(d)
    });
    r.add(m, "strcat", |c| {
        let (d, s) = (c.arg(0), c.arg(1));
        let mut v = c.host.read_cstr(d).unwrap_or_default();
        v.extend(c.host.read_cstr(s).unwrap_or_default());
        let _ = c.host.write_cstr(d, &v);
        ok(d)
    });
    r.add(m, "strcmp", |c| {
        let (a, b) = (c.host.read_cstr(c.arg(0)).unwrap_or_default(), c.host.read_cstr(c.arg(1)).unwrap_or_default());
        ok(a.cmp(&b) as i32 as u32)
    });
    r.add(m, "_stricmp", |c| {
        let a = c.host.read_cstr(c.arg(0)).unwrap_or_default().to_ascii_lowercase();
        let b = c.host.read_cstr(c.arg(1)).unwrap_or_default().to_ascii_lowercase();
        ok(a.cmp(&b) as i32 as u32)
    });
    r.add(m, "strchr", |c| {
        let (p, ch) = (c.arg(0), c.arg(1) as u8);
        let s = c.host.read_cstr(p).unwrap_or_default();
        if ch == 0 {
            return ok(p + s.len() as u32);
        }
        ok(s.iter().position(|&b| b == ch).map(|i| p + i as u32).unwrap_or(0))
    });
    r.add(m, "strrchr", |c| {
        let (p, ch) = (c.arg(0), c.arg(1) as u8);
        let s = c.host.read_cstr(p).unwrap_or_default();
        if ch == 0 {
            return ok(p + s.len() as u32);
        }
        ok(s.iter().rposition(|&b| b == ch).map(|i| p + i as u32).unwrap_or(0))
    });
    r.add(m, "toupper", |c| {
        let v = c.arg(0);
        ok(if v < 0x80 { (v as u8).to_ascii_uppercase() as u32 } else { v })
    });
    r.add(m, "tolower", |c| {
        let v = c.arg(0);
        ok(if v < 0x80 { (v as u8).to_ascii_lowercase() as u32 } else { v })
    });
    r.add(m, "atoi", |c| {
        let s = c.host.read_cstr_lossy(c.arg(0));
        let t = s.trim_start();
        let end = t.char_indices().take_while(|(i, ch)| ch.is_ascii_digit() || (*i == 0 && (*ch == '-' || *ch == '+'))).count();
        ok(t[..end].parse::<i64>().unwrap_or(0) as i32 as u32)
    });
    r.add(m, "sprintf", |c| {
        let (buf, fmt) = (c.arg(0), c.host.read_cstr(c.arg(1)).unwrap_or_default());
        let va_at = c.varargs(2);
        let mut va = VaList::new(c, va_at);
        let mut out = format(c, &fmt, &mut va, false);
        let n = out.len() as u32;
        out.push(0);
        let _ = c.host.write(buf, &out);
        ok(n)
    });
    r.add(m, "_snprintf", |c| {
        let (buf, cap, fmt) = (c.arg(0), c.arg(1), c.host.read_cstr(c.arg(2)).unwrap_or_default());
        let va_at = c.varargs(3);
        let mut va = VaList::new(c, va_at);
        let out = format(c, &fmt, &mut va, false);
        let n = out.len().min(cap as usize);
        let _ = c.host.write(buf, &out[..n]);
        if out.len() < cap as usize {
            let _ = c.host.write_u8(buf + out.len() as u32, 0);
            ok(out.len() as u32)
        } else {
            ok(0xFFFF_FFFF)
        }
    });
    r.add(m, "vsprintf", |c| {
        let (buf, fmt, va) = (c.arg(0), c.host.read_cstr(c.arg(1)).unwrap_or_default(), c.arg(2));
        let mut va = VaList::new(c, va);
        let mut out = format(c, &fmt, &mut va, false);
        let n = out.len() as u32;
        out.push(0);
        let _ = c.host.write(buf, &out);
        ok(n)
    });

    // ---- stdio ----------------------------------------------------------------------
    r.add(m, "fopen", |c| {
        let (path, mode) = (c.str_arg(0), c.str_arg(1));
        let f = fopen(c, &path, &mode);
        c.trace(format!("({path:?}, {mode:?}) = {f:#x}"));
        ok(f)
    });
    r.add(m, "fclose", |c| {
        let f = c.arg(0);
        match c.p.crt_files.remove(&f) {
            Some(h) => {
                c.p.handles.remove(&h);
                c.p.heap_free(f);
                ok(0)
            }
            None => ok(0xFFFF_FFFF),
        }
    });
    r.add(m, "fread", |c| {
        let (buf, size, count, f) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let Some(h) = file_handle(c, f) else { return ok(0) };
        let total = size.saturating_mul(count);
        let got = super::kernel32::read_file(c, h, buf, total).unwrap_or(0);
        ok(if size == 0 { 0 } else { got / size })
    });
    r.add(m, "fwrite", |c| {
        let (buf, size, count, f) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let Some(h) = file_handle(c, f) else { return ok(0) };
        let data = read_bytes(c, buf, size.saturating_mul(count));
        let n = super::kernel32::write_file(c, h, &data);
        ok(if size == 0 { 0 } else { n / size })
    });
    r.add(m, "fseek", |c| {
        let (f, off, whence) = (c.arg(0), c.arg_i32(1), c.arg(2));
        let Some(h) = file_handle(c, f) else { return ok(0xFFFF_FFFF) };
        ok(if super::kernel32::seek_file(c, h, off as i64, whence).is_some() { 0 } else { 0xFFFF_FFFF })
    });
    r.add(m, "ftell", |c| {
        let f = c.arg(0);
        let Some(h) = file_handle(c, f) else { return ok(0xFFFF_FFFF) };
        ok(super::kernel32::seek_file(c, h, 0, 1).map(|p| p as u32).unwrap_or(0xFFFF_FFFF))
    });
    r.add(m, "fprintf", |c| {
        let (f, fmt) = (c.arg(0), c.host.read_cstr(c.arg(1)).unwrap_or_default());
        let va_at = c.varargs(2);
        let mut va = VaList::new(c, va_at);
        let out = format(c, &fmt, &mut va, false);
        match file_handle(c, f) {
            Some(h) => {
                super::kernel32::write_file(c, h, &out);
            }
            None => c.p.log(format!("fprintf: {}", String::from_utf8_lossy(&out).trim_end())),
        }
        ok(out.len() as u32)
    });
    r.add(m, "printf", |c| {
        let fmt = c.host.read_cstr(c.arg(0)).unwrap_or_default();
        let va_at = c.varargs(1);
        let mut va = VaList::new(c, va_at);
        let out = format(c, &fmt, &mut va, false);
        c.p.log(format!("printf: {}", String::from_utf8_lossy(&out).trim_end()));
        ok(out.len() as u32)
    });
    r.add(m, "perror", |c| {
        let s = c.str_arg(0);
        c.p.log(format!("perror: {s}"));
        ok(0)
    });
    r.add(m, "_errno", |c| {
        let a = errno_addr(c);
        ok(a)
    });
    r.add(m, "exit", crt_exit);
    r.add(m, "_exit", crt_exit);
    r.add(m, "abort", |c| Err(ApiError(format!("msvcrt abort() called from {:#x}", c.host.read_u32(c.regs.esp).unwrap_or(0)))));
    r.add(m, "qsort", |c| {
        let (base, num, w, f) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        qsort(c, base, num, w, f)?;
        ok(0)
    });
    r.add(m, "rand", |c| {
        // MSVC LCG with a per-process seed.
        c.p.crt_rand = c.p.crt_rand.wrapping_mul(214_013).wrapping_add(2_531_011);
        ok((c.p.crt_rand >> 16) & 0x7FFF)
    });
    r.add(m, "srand", |c| {
        c.p.crt_rand = c.arg(0);
        ok(0)
    });
    r.add(m, "time", |c| {
        let t = (c.p.epoch_unix + c.p.clock_us / 1_000_000) as u32;
        let p = c.arg(0);
        if p != 0 {
            let _ = c.host.write_u32(p, t);
        }
        ok(t)
    });
    r.add(m, "clock", |c| ok(c.p.now_ms()));
    // A C++ frame handler only acts on C++ exceptions; anything else keeps searching.
    r.add(m, "__CxxFrameHandler", |c| {
        let rec = c.arg(0);
        let code = c.host.read_u32(rec).unwrap_or(0);
        if code == 0xE06D_7363 {
            return Err(ApiError("C++ exception handling (__CxxFrameHandler) is not implemented".into()));
        }
        ok(1)
    });

    // ---- math (results in ST0) -------------------------------------------------------------
    for (name, f) in [
        ("log10", libm::log10 as fn(f64) -> f64),
        ("log", libm::log),
        ("exp", libm::exp),
        ("sqrt", libm::sqrt),
        ("sin", libm::sin),
        ("cos", libm::cos),
        ("tan", libm::tan),
        ("asin", libm::asin),
        ("acos", libm::acos),
        ("atan", libm::atan),
        ("floor", libm::floor),
        ("ceil", libm::ceil),
        ("fabs", libm::fabs),
        ("sinh", libm::sinh),
        ("cosh", libm::cosh),
        ("tanh", libm::tanh),
    ] {
        r.add_math(m, name, f);
    }
    r.add(m, "pow", |c| {
        let (x, y) = (arg_f64(c, 0), arg_f64(c, 2));
        ret_f64(c, libm::pow(x, y))
    });
    r.add(m, "atan2", |c| {
        let (y, x) = (arg_f64(c, 0), arg_f64(c, 2));
        ret_f64(c, libm::atan2(y, x))
    });
    r.add(m, "fmod", |c| {
        let (x, y) = (arg_f64(c, 0), arg_f64(c, 2));
        ret_f64(c, libm::fmod(x, y))
    });
    r.add(m, "ldexp", |c| {
        let (x, e) = (arg_f64(c, 0), c.arg_i32(2));
        ret_f64(c, libm::ldexp(x, e))
    });
    r.add(m, "frexp", |c| {
        let (x, pe) = (arg_f64(c, 0), c.arg(2));
        let (m_, e) = frexp(x);
        let _ = c.host.write_u32(pe, e as u32);
        ret_f64(c, m_)
    });
    r.add(m, "modf", |c| {
        let (x, pi) = (arg_f64(c, 0), c.arg(2));
        let i = x.trunc();
        let _ = c.host.write_u64(pi, i.to_bits());
        ret_f64(c, x - i)
    });
    r.add(m, "_ftol", |c| {
        let v = c.host.fpu_pop();
        let i = v.trunc() as i64;
        Ok(Ret { eax: i as u32, pop: 0, resume: false, edx: Some((i >> 32) as u32) })
    });
    r.add(m, "_CIpow", |c| {
        let y = c.host.fpu_pop();
        let x = c.host.fpu_pop();
        ret_f64(c, libm::pow(x, y))
    });
    r.add(m, "_CIatan2", |c| {
        let x = c.host.fpu_pop();
        let y = c.host.fpu_pop();
        ret_f64(c, libm::atan2(y, x))
    });
    r.add(m, "_CIfmod", |c| {
        let y = c.host.fpu_pop();
        let x = c.host.fpu_pop();
        ret_f64(c, libm::fmod(x, y))
    });
    for (name, f) in [
        ("_CIsqrt", libm::sqrt as fn(f64) -> f64),
        ("_CIsin", libm::sin),
        ("_CIcos", libm::cos),
        ("_CItan", libm::tan),
        ("_CIasin", libm::asin),
        ("_CIacos", libm::acos),
        ("_CIatan", libm::atan),
        ("_CIexp", libm::exp),
        ("_CIlog", libm::log),
        ("_CIlog10", libm::log10),
        ("_CIsinh", libm::sinh),
        ("_CIcosh", libm::cosh),
        ("_CItanh", libm::tanh),
    ] {
        r.add_ci(m, name, f);
    }
}

fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7FF) as i32;
    if exp == 0 {
        let (m, e) = frexp(x * 18_446_744_073_709_551_616.0);
        return (m, e - 64);
    }
    let m = f64::from_bits((bits & !(0x7FFu64 << 52)) | (1022u64 << 52));
    (m, exp - 1022)
}

impl Registry {
    /// `double f(double)` taking its argument on the stack.
    fn add_math(&mut self, dll: &str, name: &str, f: fn(f64) -> f64) {
        let id = self.add(dll, name, |c| {
            let f = c.p.registry.math_fns[&c.id.0];
            let x = arg_f64(c, 0);
            ret_f64(c, f(x))
        });
        self.math_fns.insert(id.0, f);
    }

    /// `_CIxxx`: operand in ST(0), result replaces it.
    fn add_ci(&mut self, dll: &str, name: &str, f: fn(f64) -> f64) {
        let id = self.add(dll, name, |c| {
            let f = c.p.registry.math_fns[&c.id.0];
            let x = c.host.fpu_pop();
            ret_f64(c, f(x))
        });
        self.math_fns.insert(id.0, f);
    }
}

#[allow(dead_code)]
fn handle_is_file(c: &Ctx, h: u32) -> bool {
    matches!(c.p.handles.get(&h), Some(Handle::File(_)))
}

// ---- the rest of what game DLLs (d3dx9_43.dll) import --------------------------------

/// strtod over guest memory: the longest prefix that parses (decimal, hex floats,
/// inf/nan); returns the value and the number of bytes consumed (0: no number).
fn parse_double(c: &Ctx, p: u32) -> (f64, u32) {
    let s = c.host.read_cstr(p).unwrap_or_default();
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || (9..=13).contains(&s[i])) {
        i += 1;
    }
    let start = i;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        i += 1;
    }
    let lower: Vec<u8> = s[i..].iter().map(|b| b.to_ascii_lowercase()).collect();
    let neg = start < s.len() && s[start] == b'-';
    let sign = if neg { -1.0 } else { 1.0 };
    for (word, v) in [(&b"infinity"[..], f64::INFINITY), (b"inf", f64::INFINITY), (b"nan", f64::NAN)] {
        if lower.starts_with(word) {
            return (sign * v, (i + word.len()) as u32);
        }
    }
    let digits_start = i;
    let mut saw_digit = false;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
        saw_digit = true;
    }
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return (0.0, 0);
    }
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        let mut j = i + 1;
        if j < s.len() && (s[j] == b'+' || s[j] == b'-') {
            j += 1;
        }
        if j < s.len() && s[j].is_ascii_digit() {
            while j < s.len() && s[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    let text = std::str::from_utf8(&s[digits_start..i]).unwrap_or("0");
    (sign * text.parse::<f64>().unwrap_or(0.0), i as u32)
}

/// strtol-style integer prefix: (value, bytes consumed).
fn parse_long(s: &[u8], base: u32) -> (i64, usize) {
    let mut i = 0;
    while i < s.len() && (s[i] == b' ' || (9..=13).contains(&s[i])) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut base = base;
    if (base == 0 || base == 16) && i + 1 < s.len() && s[i] == b'0' && (s[i + 1] | 0x20) == b'x' {
        i += 2;
        base = 16;
    } else if base == 0 && i < s.len() && s[i] == b'0' {
        base = 8;
    } else if base == 0 {
        base = 10;
    }
    let digits = i;
    let mut v: i64 = 0;
    while i < s.len() {
        let d = match s[i] {
            b'0'..=b'9' => (s[i] - b'0') as u32,
            b'a'..=b'z' => (s[i] - b'a') as u32 + 10,
            b'A'..=b'Z' => (s[i] - b'A') as u32 + 10,
            _ => break,
        };
        if d >= base {
            break;
        }
        v = v.wrapping_mul(base as i64).wrapping_add(d as i64);
        i += 1;
    }
    if i == digits {
        return (0, 0);
    }
    (if neg { v.wrapping_neg() } else { v }, i)
}

/// sscanf over guest memory with the conversions C code commonly uses.
fn scan(c: &mut Ctx, input: &[u8], fmt: &[u8], va: &mut VaList) -> i32 {
    let (mut si, mut fi, mut assigned) = (0usize, 0usize, 0i32);
    let ws = |b: u8| b == b' ' || (9..=13).contains(&b);
    while fi < fmt.len() {
        let f = fmt[fi];
        if ws(f) {
            while si < input.len() && ws(input[si]) {
                si += 1;
            }
            fi += 1;
            continue;
        }
        if f != b'%' {
            if si >= input.len() || input[si] != f {
                break;
            }
            si += 1;
            fi += 1;
            continue;
        }
        fi += 1;
        if fi < fmt.len() && fmt[fi] == b'%' {
            if si >= input.len() || input[si] != b'%' {
                break;
            }
            si += 1;
            fi += 1;
            continue;
        }
        let suppress = fi < fmt.len() && fmt[fi] == b'*';
        if suppress {
            fi += 1;
        }
        let mut width = 0usize;
        while fi < fmt.len() && fmt[fi].is_ascii_digit() {
            width = width * 10 + (fmt[fi] - b'0') as usize;
            fi += 1;
        }
        let mut size = 0; // 1 = h, 2 = l, 3 = ll/I64
        while fi < fmt.len() && matches!(fmt[fi], b'h' | b'l' | b'L' | b'I' | b'6' | b'4') {
            size = match fmt[fi] {
                b'h' => 1,
                b'l' if size == 2 => 3,
                b'l' | b'L' => 2,
                b'4' => 3,
                _ => size,
            };
            fi += 1;
        }
        let Some(&conv) = fmt.get(fi) else { break };
        fi += 1;
        if !matches!(conv, b'c' | b'[' | b'n') {
            while si < input.len() && ws(input[si]) {
                si += 1;
            }
        }
        let limit = |si: usize| if width > 0 { (si + width).min(input.len()) } else { input.len() };
        match conv {
            b'd' | b'i' | b'u' | b'x' | b'X' | b'o' => {
                let base = match conv {
                    b'd' | b'u' => 10,
                    b'x' | b'X' => 16,
                    b'o' => 8,
                    _ => 0,
                };
                let (v, n) = parse_long(&input[si..limit(si)], base);
                if n == 0 {
                    break;
                }
                si += n;
                if !suppress {
                    let p = va.u32(c);
                    let _ = match size {
                        1 => c.host.write_u16(p, v as u16),
                        3 => c.host.write_u64(p, v as u64),
                        _ => c.host.write_u32(p, v as u32),
                    };
                    assigned += 1;
                }
            }
            b'f' | b'e' | b'g' | b'E' | b'G' => {
                let end = limit(si);
                let text = String::from_utf8_lossy(&input[si..end]).into_owned();
                // the longest prefix that is a number
                let mut best = None;
                for k in (1..=text.len()).rev() {
                    let t = &text[..k];
                    if t.ends_with(['e', 'E', '+', '-']) {
                        continue;
                    }
                    if let Ok(v) = t.parse::<f64>() {
                        best = Some((v, k));
                        break;
                    }
                }
                let Some((v, n)) = best else { break };
                si += n;
                if !suppress {
                    let p = va.u32(c);
                    let _ = if size >= 2 { c.host.write_u64(p, v.to_bits()) } else { c.host.write_u32(p, (v as f32).to_bits()) };
                    assigned += 1;
                }
            }
            b's' => {
                let start = si;
                let end = limit(si);
                while si < end && !ws(input[si]) {
                    si += 1;
                }
                if si == start {
                    break;
                }
                if !suppress {
                    let p = va.u32(c);
                    let mut b = input[start..si].to_vec();
                    b.push(0);
                    let _ = c.host.write(p, &b);
                    assigned += 1;
                }
            }
            b'c' => {
                let n = width.max(1);
                if si + n > input.len() {
                    break;
                }
                if !suppress {
                    let p = va.u32(c);
                    let _ = c.host.write(p, &input[si..si + n]);
                    assigned += 1;
                }
                si += n;
            }
            b'[' => {
                let negate = fi < fmt.len() && fmt[fi] == b'^';
                if negate {
                    fi += 1;
                }
                let set_start = fi;
                if fi < fmt.len() && fmt[fi] == b']' {
                    fi += 1;
                }
                while fi < fmt.len() && fmt[fi] != b']' {
                    fi += 1;
                }
                let set = &fmt[set_start..fi];
                fi += 1;
                let in_set = |b: u8| {
                    let mut k = 0;
                    let mut hit = false;
                    while k < set.len() {
                        if k + 2 < set.len() && set[k + 1] == b'-' {
                            hit |= (set[k]..=set[k + 2]).contains(&b);
                            k += 3;
                        } else {
                            hit |= set[k] == b;
                            k += 1;
                        }
                    }
                    hit != negate
                };
                let start = si;
                let end = limit(si);
                while si < end && in_set(input[si]) {
                    si += 1;
                }
                if si == start {
                    break;
                }
                if !suppress {
                    let p = va.u32(c);
                    let mut b = input[start..si].to_vec();
                    b.push(0);
                    let _ = c.host.write(p, &b);
                    assigned += 1;
                }
            }
            b'n' => {
                if !suppress {
                    let p = va.u32(c);
                    let _ = c.host.write_u32(p, si as u32);
                }
            }
            _ => break,
        }
    }
    if assigned == 0 && si >= input.len() && !input.is_empty() { assigned } else if input.is_empty() { -1 } else { assigned }
}

/// x87 status word bits as the CRT reports them (_SW_*).
fn sw_to_crt(sw: u16) -> u32 {
    let mut r = 0;
    for (x, crt) in [(0x20, 0x1), (0x10, 0x2), (0x08, 0x4), (0x04, 0x8), (0x01, 0x10), (0x02, 0x80000)] {
        if sw & x != 0 {
            r |= crt;
        }
    }
    r
}

/// x87 control word <-> the CRT's abstract control word (_EM_*, _RC_*, _PC_*).
fn cw_to_crt(cw: u16) -> u32 {
    let mut r = sw_to_crt(cw & 0x3f);
    r |= match (cw >> 10) & 3 {
        1 => 0x100,
        2 => 0x200,
        3 => 0x300,
        _ => 0,
    };
    r |= match (cw >> 8) & 3 {
        0 => 0x20000,
        2 => 0x10000,
        _ => 0,
    };
    if cw & 0x1000 != 0 {
        r |= 0x40000;
    }
    r
}

fn crt_to_cw(v: u32, old: u16) -> u16 {
    let mut cw = old & !0x1f3f;
    for (x, crt) in [(0x20, 0x1), (0x10, 0x2), (0x08, 0x4), (0x04, 0x8), (0x01, 0x10), (0x02, 0x80000)] {
        if v & crt != 0 {
            cw |= x;
        }
    }
    cw |= match v & 0x300 {
        0x100 => 1 << 10,
        0x200 => 2 << 10,
        0x300 => 3 << 10,
        _ => 0,
    };
    cw |= match v & 0x30000 {
        0x20000 => 0,
        0x10000 => 2 << 8,
        _ => 3 << 8,
    };
    if v & 0x40000 != 0 {
        cw |= 0x1000;
    }
    cw
}

fn itoa(v: u64, neg: bool, radix: u32) -> Vec<u8> {
    let radix = radix.clamp(2, 36) as u64;
    let mut digits = Vec::new();
    let mut n = v;
    loop {
        let d = (n % radix) as u8;
        digits.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        n /= radix;
        if n == 0 {
            break;
        }
    }
    if neg {
        digits.push(b'-');
    }
    digits.reverse();
    digits
}

/// _vsnwprintf: the wide format string formatted narrow (%s takes wide strings), then
/// widened into `buf` (`cap` units; -1 when it does not fit with its terminator).
fn snwprintf(c: &mut Ctx, buf: u32, cap: u32, fmt: u32, va: u32) -> u32 {
    let f = c.host.read_wstr(fmt).unwrap_or_default();
    let mut va = VaList::new(c, va);
    let out = format(c, f.as_bytes(), &mut va, true);
    let text: Vec<u16> = String::from_utf8_lossy(&out).encode_utf16().collect();
    let n = text.len().min(cap as usize);
    for (i, u) in text[..n].iter().enumerate() {
        let _ = c.host.write_u16(buf + 2 * i as u32, *u);
    }
    if text.len() < cap as usize {
        let _ = c.host.write_u16(buf + 2 * text.len() as u32, 0);
        text.len() as u32
    } else {
        0xFFFF_FFFF
    }
}

fn write_cstr(c: &mut Ctx, p: u32, s: &[u8]) {
    let mut b = s.to_vec();
    b.push(0);
    let _ = c.host.write(p, &b);
}

fn strdup(c: &mut Ctx, s: &[u8]) -> u32 {
    let p = alloc(c, s.len() as u32 + 1, false);
    write_cstr(c, p, s);
    p
}

/// FILE structs for stdin/stdout/stderr and the rest of the table (32 bytes each).
fn iob_init(_base: u32) -> Vec<u8> {
    let mut v = vec![0u8; 20 * 32];
    for (i, flag) in [(0usize, 1u32), (1, 2), (2, 2)] {
        v[i * 32 + 12..i * 32 + 16].copy_from_slice(&flag.to_le_bytes());
        v[i * 32 + 16..i * 32 + 20].copy_from_slice(&(i as u32).to_le_bytes());
    }
    v
}

/// __pioinfo: 64 pointers to blocks of 32 low-level file entries (none open).
fn pioinfo_init(base: u32) -> Vec<u8> {
    let mut v = vec![0u8; 256 + 32 * 64];
    for i in 0..64 {
        v[4 * i..4 * i + 4].copy_from_slice(&(base + 256).to_le_bytes());
    }
    for k in 0..32 {
        v[256 + 64 * k..256 + 64 * k + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    }
    v
}

/// __badioinfo: an entry with no OS handle.
fn badioinfo_init(_base: u32) -> Vec<u8> {
    let mut v = vec![0u8; 64];
    v[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    v
}

/// One of msvcrt's exported variables, allocated (and initialized) on first use.
fn crt_var(c: &mut Ctx, name: &str) -> u32 {
    let a = c.p.registry.data_export("msvcrt.dll", name).unwrap_or(0);
    let g: &mut dyn Guest = &mut *c.host;
    c.p.registry.flush_data(g);
    a
}

/// __initenv: a pointer to an empty environment block that follows it.
fn initenv_init(base: u32) -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v[..4].copy_from_slice(&(base + 4).to_le_bytes());
    v
}

/// struct lconv of the "C" locale: ten string pointers, eight CHAR_MAX fields, then
/// the strings "." and "".
fn lconv_init(base: u32) -> Vec<u8> {
    let mut v = vec![0u8; 52];
    for i in 0..10u32 {
        let s = if i == 0 { base + 48 } else { base + 50 };
        v[4 * i as usize..4 * i as usize + 4].copy_from_slice(&s.to_le_bytes());
    }
    v[40..48].fill(127);
    v[48] = b'.';
    v
}

fn strerror_text(e: u32) -> &'static str {
    match e {
        0 => "No error",
        1 => "Operation not permitted",
        2 => "No such file or directory",
        5 => "Input/output error",
        9 => "Bad file descriptor",
        12 => "Not enough space",
        13 => "Permission denied",
        17 => "File exists",
        22 => "Invalid argument",
        24 => "Too many open files",
        28 => "No space left on device",
        33 => "Domain error",
        34 => "Result too large",
        _ => "Unknown error",
    }
}

pub fn register_more(r: &mut Registry) {
    let m = "msvcrt.dll";
    r.add_data(m, "_iob", 20 * 32, iob_init);
    // What mingw's (and old MSVC's) startup code asks of msvcrt.
    r.add_data(m, "__initenv", 8, initenv_init);
    r.add_data(m, "_commode", 4, |_| vec![0; 4]);
    r.add_data(m, "_fmode", 4, |_| vec![0; 4]);
    r.add_data(m, "__mb_cur_max", 4, |_| 1u32.to_le_bytes().to_vec());
    r.add_data(m, "__lconv", 52, lconv_init);
    r.add_data(m, "__strerror_buf", 64, |_| vec![0; 64]);
    r.add(m, "__set_app_type", |_c| ok(0));
    r.add(m, "__setusermatherr", |_c| ok(0));
    r.add(m, "_cexit", |_c| ok(0));
    r.add(m, "__p___initenv", |c| ok(crt_var(c, "__initenv")));
    r.add(m, "__p__commode", |c| ok(crt_var(c, "_commode")));
    r.add(m, "__p__fmode", |c| ok(crt_var(c, "_fmode")));
    r.add(m, "__p___mb_cur_max", |c| ok(crt_var(c, "__mb_cur_max")));
    r.add(m, "__p__iob", |c| ok(crt_var(c, "_iob")));
    r.add(m, "__iob_func", |c| ok(crt_var(c, "_iob")));
    r.add(m, "localeconv", |c| ok(crt_var(c, "__lconv")));
    r.add(m, "__getmainargs", |c| {
        // argc, argv = { the exe's path }, envp = the empty environment
        let (pargc, pargv, penv) = (c.arg(0), c.arg(1), c.arg(2));
        let path = c.p.exe_path.clone();
        let s = strdup(c, path.as_bytes());
        let argv = alloc(c, 8, true);
        let _ = c.host.write_u32(argv, s);
        let env = crt_var(c, "__initenv");
        let envp = c.host.read_u32(env).unwrap_or(0);
        let _ = c.host.write_u32(pargc, 1);
        let _ = c.host.write_u32(pargv, argv);
        let _ = c.host.write_u32(penv, envp);
        ok(0)
    });
    r.add(m, "signal", |_c| ok(0)); // SIG_DFL
    r.add(m, "strerror", |c| {
        let buf = crt_var(c, "__strerror_buf");
        let text = strerror_text(c.arg(0));
        write_cstr(c, buf, text.as_bytes());
        ok(buf)
    });
    r.add(m, "strncmp", |c| {
        let (a, b, n) = (c.arg(0), c.arg(1), c.arg(2));
        for i in 0..n {
            let (x, y) = (c.host.read_u8(a + i).unwrap_or(0), c.host.read_u8(b + i).unwrap_or(0));
            if x != y {
                return ok(if x < y { u32::MAX } else { 1 });
            }
            if x == 0 {
                break;
            }
        }
        ok(0)
    });
    r.add(m, "wcslen", |c| {
        let p = c.arg(0);
        let mut n = 0;
        while c.host.read_u16(p + 2 * n).unwrap_or(0) != 0 {
            n += 1;
        }
        ok(n)
    });
    r.add(m, "puts", |c| {
        let s = c.str_arg(0);
        c.p.log(format!("puts: {s}"));
        ok(0)
    });
    r.add(m, "fputc", |c| {
        let (ch, f) = (c.arg(0), c.arg(1));
        match file_handle(c, f) {
            Some(h) => {
                super::kernel32::write_file(c, h, &[ch as u8]);
            }
            None => {
                // stdout/stderr: logged a line at a time
                if ch as u8 == b'\n' {
                    let line = String::from_utf8_lossy(&std::mem::take(&mut c.p.crt_line)).into_owned();
                    c.p.log(format!("stdout: {line}"));
                } else {
                    c.p.crt_line.push(ch as u8);
                }
            }
        }
        ok(ch & 0xFF)
    });
    r.add(m, "vfprintf", |c| {
        let (f, fmt, va) = (c.arg(0), c.host.read_cstr(c.arg(1)).unwrap_or_default(), c.arg(2));
        let mut va = VaList::new(c, va);
        let out = format(c, &fmt, &mut va, false);
        match file_handle(c, f) {
            Some(h) => {
                super::kernel32::write_file(c, h, &out);
            }
            None => c.p.log(format!("fprintf: {}", String::from_utf8_lossy(&out).trim_end())),
        }
        ok(out.len() as u32)
    });
    r.add_data(m, "__pioinfo", 256 + 32 * 64, pioinfo_init);
    r.add_data(m, "__badioinfo", 64, badioinfo_init);

    // ---- startup and shutdown of DLLs that link the CRT -----------------------------
    r.add(m, "_initterm", |c| {
        let (mut p, end) = (c.arg(0), c.arg(1));
        while p < end {
            let f = c.host.read_u32(p).unwrap_or(0);
            if f != 0 {
                c.call_guest(f, &[])?;
            }
            p += 4;
        }
        ok(0)
    });
    r.add(m, "_initterm_e", |c| {
        let (mut p, end) = (c.arg(0), c.arg(1));
        while p < end {
            let f = c.host.read_u32(p).unwrap_or(0);
            if f != 0 {
                let r = c.call_guest(f, &[])?;
                if r != 0 {
                    return ok(r);
                }
            }
            p += 4;
        }
        ok(0)
    });
    // Termination tables are never run: processes end with the page.
    r.add(m, "_onexit", |c| ok(c.arg(0)));
    r.add(m, "__dllonexit", |c| ok(c.arg(0)));
    r.add(m, "atexit", |_c| ok(0));
    r.add(m, "_XcptFilter", |_c| ok(0));
    r.add(m, "_lock", |_c| ok(0));
    r.add(m, "_unlock", |_c| ok(0));
    r.add(m, "_amsg_exit", |c| Err(ApiError(format!("msvcrt runtime error R60{:02}", c.arg(0)))));
    r.add(m, "_purecall", |c| Err(ApiError(format!("pure virtual function call from {:#x}", c.host.read_u32(c.regs.esp).unwrap_or(0)))));
    r.add(m, "?terminate@@YAXXZ", |c| Err(ApiError(format!("terminate() called from {:#x}", c.host.read_u32(c.regs.esp).unwrap_or(0)))));

    // ---- floating-point environment ---------------------------------------------------
    r.add(m, "_controlfp", |c| {
        let (new, mask) = (c.arg(0), c.arg(1));
        let mut f = c.host.fpu_state();
        let cur = cw_to_crt(f.control_word());
        if mask != 0 {
            let v = (cur & !mask) | (new & mask);
            f.set_control_word(crt_to_cw(v, f.control_word()));
            c.host.set_fpu_state(&f);
            // SSE follows the exception masks and rounding mode
            let mx = c.host.mxcsr();
            let em = ((f.control_word() & 0x3f) as u32) << 7;
            let rc = (((f.control_word() >> 10) & 3) as u32) << 13;
            c.host.set_mxcsr((mx & !(0x3f << 7) & !(3 << 13)) | em | rc);
        }
        ok(cw_to_crt(f.control_word()))
    });
    r.alias(m, "_control87", m, "_controlfp");
    r.add(m, "_clearfp", |c| {
        let mut f = c.host.fpu_state();
        let old = sw_to_crt(f.status_word());
        f.set_status_word(f.status_word() & !0x80ff);
        c.host.set_fpu_state(&f);
        let mx = c.host.mxcsr();
        c.host.set_mxcsr(mx & !0x3f);
        ok(old)
    });
    r.add(m, "_statusfp", |c| {
        let f = c.host.fpu_state();
        ok(sw_to_crt(f.status_word()))
    });
    r.add(m, "_fpreset", |c| {
        let mut f = c.host.fpu_state();
        f.set_control_word(0x027f);
        f.set_status_word(0);
        c.host.set_fpu_state(&f);
        ok(0)
    });
    r.add(m, "_finite", |c| {
        let v = arg_f64(c, 0);
        ok(v.is_finite() as u32)
    });
    r.add(m, "_isnan", |c| {
        let v = arg_f64(c, 0);
        ok(v.is_nan() as u32)
    });

    // ---- numbers and strings ----------------------------------------------------------
    r.add(m, "atof", |c| {
        let (v, _) = parse_double(c, c.arg(0));
        ret_f64(c, v)
    });
    r.add(m, "strtod", |c| {
        let (s, end) = (c.arg(0), c.arg(1));
        let (v, n) = parse_double(c, s);
        if end != 0 {
            let _ = c.host.write_u32(end, s + n);
        }
        ret_f64(c, v)
    });
    r.add(m, "atol", |c| {
        let s = c.host.read_cstr(c.arg(0)).unwrap_or_default();
        ok(parse_long(&s, 10).0 as i32 as u32)
    });
    r.add(m, "strtol", |c| {
        let (p, end, base) = (c.arg(0), c.arg(1), c.arg(2));
        let s = c.host.read_cstr(p).unwrap_or_default();
        let (v, n) = parse_long(&s, base);
        if end != 0 {
            let _ = c.host.write_u32(end, p + n as u32);
        }
        ok(v.clamp(i32::MIN as i64, i32::MAX as i64) as i32 as u32)
    });
    r.add(m, "strtoul", |c| {
        let (p, end, base) = (c.arg(0), c.arg(1), c.arg(2));
        let s = c.host.read_cstr(p).unwrap_or_default();
        let (v, n) = parse_long(&s, base);
        if end != 0 {
            let _ = c.host.write_u32(end, p + n as u32);
        }
        ok(v as u32)
    });
    r.add(m, "_itoa", |c| {
        let (v, buf, radix) = (c.arg_i32(0), c.arg(1), c.arg(2));
        let s = if radix == 10 { itoa(v.unsigned_abs() as u64, v < 0, 10) } else { itoa(v as u32 as u64, false, radix) };
        write_cstr(c, buf, &s);
        ok(buf)
    });
    r.add(m, "_ultoa", |c| {
        let (v, buf, radix) = (c.arg(0), c.arg(1), c.arg(2));
        let s = itoa(v as u64, false, radix);
        write_cstr(c, buf, &s);
        ok(buf)
    });
    r.add(m, "_strdup", |c| {
        let p = c.arg(0);
        if p == 0 {
            return ok(0);
        }
        let s = c.host.read_cstr(p).unwrap_or_default();
        let d = strdup(c, &s);
        ok(d)
    });
    r.add(m, "_strdate", |c| {
        let buf = c.arg(0);
        let t = super::kernel32::host_systemtime(c);
        let s = format!("{:02}/{:02}/{:02}", t[1], t[3], t[0] % 100);
        write_cstr(c, buf, s.as_bytes());
        ok(buf)
    });
    r.add(m, "_strtime", |c| {
        let buf = c.arg(0);
        let t = super::kernel32::host_systemtime(c);
        let s = format!("{:02}:{:02}:{:02}", t[4], t[5], t[6]);
        write_cstr(c, buf, s.as_bytes());
        ok(buf)
    });
    r.add(m, "_tempnam", |c| {
        let prefix = if c.arg(1) != 0 { c.host.read_cstr_lossy(c.arg(1)) } else { String::new() };
        c.p.crt_rand = c.p.crt_rand.wrapping_add(1);
        let name = format!("{}\\{prefix}{:x}", crate::process::TEMP_DIR, c.p.crt_rand);
        let p = strdup(c, name.as_bytes());
        ok(p)
    });
    r.add(m, "_vsnprintf", |c| {
        let (buf, cap, fmt, va) = (c.arg(0), c.arg(1), c.host.read_cstr(c.arg(2)).unwrap_or_default(), c.arg(3));
        let mut va = VaList::new(c, va);
        let out = format(c, &fmt, &mut va, false);
        let n = out.len().min(cap as usize);
        let _ = c.host.write(buf, &out[..n]);
        if out.len() < cap as usize {
            let _ = c.host.write_u8(buf + out.len() as u32, 0);
            ok(out.len() as u32)
        } else {
            ok(0xFFFF_FFFF)
        }
    });
    r.add(m, "_vsnwprintf", |c| {
        let (buf, cap, fmt, va) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        ok(snwprintf(c, buf, cap, fmt, va))
    });
    r.add(m, "_snwprintf", |c| {
        let (buf, cap, fmt) = (c.arg(0), c.arg(1), c.arg(2));
        let va = c.regs.esp + 16;
        ok(snwprintf(c, buf, cap, fmt, va))
    });
    r.add(m, "sscanf", |c| {
        let input = c.host.read_cstr(c.arg(0)).unwrap_or_default();
        let fmt = c.host.read_cstr(c.arg(1)).unwrap_or_default();
        let va_at = c.varargs(2);
        let mut va = VaList::new(c, va_at);
        let n = scan(c, &input, &fmt, &mut va);
        ok(n as u32)
    });
    r.add(m, "setlocale", |c| {
        let want = if c.arg(1) != 0 { c.host.read_cstr_lossy(c.arg(1)) } else { "C".into() };
        if !(want.is_empty() || want == "C") {
            return ok(0);
        }
        if c.p.crt_locale == 0 {
            c.p.crt_locale = strdup(c, b"C");
        }
        ok(c.p.crt_locale)
    });
    r.add(m, "wcstombs", |c| {
        let (dst, src, n) = (c.arg(0), c.arg(1), c.arg(2));
        let s = c.host.read_wstr(src).unwrap_or_default();
        let bytes = super::kernel32::encode_cp(1252, &s);
        if dst == 0 {
            return ok(bytes.len() as u32);
        }
        let k = bytes.len().min(n as usize);
        let _ = c.host.write(dst, &bytes[..k]);
        if k < n as usize {
            let _ = c.host.write_u8(dst + k as u32, 0);
        }
        ok(k as u32)
    });
    r.add(m, "wcstoul", |c| {
        let (p, end, base) = (c.arg(0), c.arg(1), c.arg(2));
        let w = c.host.read_wstr(p).unwrap_or_default();
        // digits are ASCII: one byte per character
        let s: Vec<u8> = w.chars().map(|ch| if ch.is_ascii() { ch as u8 } else { 0xFF }).collect();
        let (v, n) = parse_long(&s, base);
        if end != 0 {
            let _ = c.host.write_u32(end, p + 2 * n as u32);
        }
        ok(v as u32)
    });
    r.add(m, "abs", |c| ok(c.arg_i32(0).wrapping_abs() as u32));
    r.alias(m, "labs", m, "abs");
    r.add_data(m, "_adjust_fdiv", 4, |_| vec![0; 4]);
    r.add(m, "getenv", |c| {
        let name = c.str_arg(0);
        let Some(v) = c.p.env.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone()) else { return ok(0) };
        let key = name.to_ascii_uppercase();
        if let Some(&p) = c.p.crt_getenv.get(&key) {
            if c.host.read_cstr_lossy(p) == v {
                return ok(p);
            }
        }
        let p = strdup(c, &super::kernel32::encode_cp(1252, &v));
        c.p.crt_getenv.insert(key, p);
        ok(p)
    });
    r.add(m, "_fullpath", |c| {
        let (buf, rel, cap) = (c.arg(0), c.arg(1), c.arg(2));
        let rel = if rel == 0 { String::new() } else { c.str_arg(1) };
        let full = super::kernel32::full_path(c, &rel);
        let bytes = super::kernel32::encode_cp(1252, &full);
        if buf == 0 {
            return ok(strdup(c, &bytes));
        }
        if bytes.len() >= cap as usize {
            set_errno(c, 34); // ERANGE
            return ok(0);
        }
        write_cstr(c, buf, &bytes);
        ok(buf)
    });
    r.add(m, "_stat", |c| {
        let (path, out) = (c.str_arg(0), c.arg(1));
        let Some(meta) = c.p.fs.stat(&c.p.vpath(&path)) else {
            set_errno(c, 2); // ENOENT
            return ok(0xFFFF_FFFF);
        };
        // struct _stat with 32-bit time_t: dev, ino:u16, mode:u16, nlink, uid, gid, rdev, size, atime, mtime, ctime
        let exe = [".exe", ".com", ".bat", ".cmd"].iter().any(|e| path.to_ascii_lowercase().ends_with(e));
        let mut mode: u32 = if meta.is_dir { 0x4000 | 0x1C0 } else { 0x8000 | 0x180 | if exe { 0x40 } else { 0 } };
        mode |= (mode & 0x1C0) >> 3 | (mode & 0x1C0) >> 6;
        let drive = path.as_bytes().get(1).filter(|&&b| b == b':').map(|_| (path.as_bytes()[0].to_ascii_uppercase() - b'A') as u32).unwrap_or(2);
        let time = if meta.mtime > 116_444_736_000_000_000 { ((meta.mtime - 116_444_736_000_000_000) / 10_000_000) as u32 } else { 0 };
        let mut b = vec![0u8; 36];
        b[0..4].copy_from_slice(&drive.to_le_bytes());
        b[6..8].copy_from_slice(&(mode as u16).to_le_bytes());
        b[8..10].copy_from_slice(&1u16.to_le_bytes());
        b[16..20].copy_from_slice(&drive.to_le_bytes());
        b[20..24].copy_from_slice(&(meta.size as u32).to_le_bytes());
        for k in 0..3 {
            b[24 + 4 * k..28 + 4 * k].copy_from_slice(&time.to_le_bytes());
        }
        let _ = c.host.write(out, &b);
        ok(0)
    });
    r.add(m, "wctomb", |c| {
        let (dst, wc) = (c.arg(0), c.arg(1) & 0xFFFF);
        if dst == 0 {
            return ok(0);
        }
        let s: String = char::from_u32(wc).map(String::from).unwrap_or_default();
        let b = super::kernel32::encode_cp(1252, &s);
        let _ = c.host.write(dst, &b);
        ok(b.len() as u32)
    });

    // ---- character classes ("C" locale; wide ones by Unicode) -------------------------
    r.add(m, "isalpha", |c| ok((c.arg(0) < 0x80 && (c.arg(0) as u8).is_ascii_alphabetic()) as u32 * 0x103));
    r.add(m, "isdigit", |c| ok((c.arg(0) < 0x80 && (c.arg(0) as u8).is_ascii_digit()) as u32 * 0x4));
    r.add(m, "isalnum", |c| ok((c.arg(0) < 0x80 && (c.arg(0) as u8).is_ascii_alphanumeric()) as u32 * 0x107));
    r.add(m, "isspace", |c| ok((c.arg(0) < 0x80 && ((c.arg(0) as u8) == b' ' || (9..=13).contains(&(c.arg(0) as u8)))) as u32 * 0x8));
    r.add(m, "isxdigit", |c| ok((c.arg(0) < 0x80 && (c.arg(0) as u8).is_ascii_hexdigit()) as u32 * 0x80));
    r.add(m, "isupper", |c| ok((c.arg(0) < 0x80 && (c.arg(0) as u8).is_ascii_uppercase()) as u32 * 0x1));
    r.add(m, "islower", |c| ok((c.arg(0) < 0x80 && (c.arg(0) as u8).is_ascii_lowercase()) as u32 * 0x2));
    r.add(m, "isprint", |c| ok((c.arg(0) < 0x80 && (0x20..0x7f).contains(&c.arg(0))) as u32 * 0x157));
    r.add(m, "ispunct", |c| ok((c.arg(0) < 0x80 && (c.arg(0) as u8).is_ascii_punctuation()) as u32 * 0x10));
    r.add(m, "iscntrl", |c| ok((c.arg(0) < 0x20 || c.arg(0) == 0x7f) as u32 * 0x20));
    r.add(m, "isleadbyte", |_c| ok(0));
    r.add(m, "iswalpha", |c| ok(char::from_u32(c.arg(0) & 0xFFFF).is_some_and(|ch| ch.is_alphabetic()) as u32 * 0x103));
    r.add(m, "iswdigit", |c| ok(char::from_u32(c.arg(0) & 0xFFFF).is_some_and(|ch| ch.is_ascii_digit()) as u32 * 0x4));
    r.add(m, "iswspace", |c| ok(char::from_u32(c.arg(0) & 0xFFFF).is_some_and(|ch| ch.is_whitespace()) as u32 * 0x8));
    r.add(m, "iswpunct", |c| ok(char::from_u32(c.arg(0) & 0xFFFF).is_some_and(|ch| ch.is_ascii_punctuation() || (!ch.is_alphanumeric() && !ch.is_whitespace() && !ch.is_control() && ch as u32 > 0x7f)) as u32 * 0x10));

    // ---- low-level I/O (only the console handles exist) --------------------------------
    r.add(m, "_fileno", |c| {
        let f = c.arg(0);
        ok(c.host.read_u32(f + 16).unwrap_or(u32::MAX))
    });
    r.add(m, "_isatty", |_c| ok(0));
    r.add(m, "_write", |c| {
        let (fd, buf, n) = (c.arg(0), c.arg(1), c.arg(2));
        if fd == 1 || fd == 2 {
            let b = read_bytes(c, buf, n);
            c.p.log(format!("msvcrt write({fd}): {}", String::from_utf8_lossy(&b).trim_end()));
            return ok(n);
        }
        set_errno(c, 9); // EBADF
        ok(u32::MAX)
    });
    r.add(m, "_lseeki64", |c| {
        set_errno(c, 9);
        Ok(Ret { edx: Some(u32::MAX), ..Ret::cdecl(u32::MAX) })
    });
    r.add(m, "fflush", |_c| ok(0));

    // ---- setjmp / longjmp ---------------------------------------------------------------
    // _JUMP_BUFFER: ebp, ebx, edi, esi, esp, eip, registration, trylevel, cookie,
    // unwind func, unwind data[6]
    r.add(m, "_setjmp3", |c| {
        let buf = c.arg(0);
        let ret_addr = c.host.read_u32(c.regs.esp).unwrap_or(0);
        let fs0 = c.host.read_u32(c.regs.fs_base).unwrap_or(u32::MAX);
        let vals = [c.regs.ebp, c.regs.ebx, c.regs.edi, c.regs.esi, c.regs.esp + 4, ret_addr, fs0, u32::MAX, 0x5633_3256, 0];
        for (i, v) in vals.iter().enumerate() {
            let _ = c.host.write_u32(buf + 4 * i as u32, *v);
        }
        ok(0)
    });
    r.alias(m, "_setjmp", m, "_setjmp3");
    r.add(m, "longjmp", |c| {
        let (buf, val) = (c.arg(0), c.arg(1));
        let rd = |c: &Ctx, i: u32| c.host.read_u32(buf + 4 * i).unwrap_or(0);
        let (ebp, ebx, edi, esi, esp, eip, reg) = (rd(c, 0), rd(c, 1), rd(c, 2), rd(c, 3), rd(c, 4), rd(c, 5), rd(c, 6));
        // Exception handlers registered after setjmp are abandoned.
        let _ = c.host.write_u32(c.regs.fs_base, reg);
        c.regs.ebp = ebp;
        c.regs.ebx = ebx;
        c.regs.edi = edi;
        c.regs.esi = esi;
        c.regs.esp = esp;
        c.regs.eip = eip;
        c.regs.eax = if val == 0 { 1 } else { val };
        Ok(Ret::resume())
    });
}

/// C runtime functions added later than the rest: made only when a module imports them
/// (`Registry::on_demand`), so the stub addresses of everything else stay put.
pub fn register_late(r: &mut Registry) {
    let m = "msvcrt.dll";
    // _strrev(s) / _wcsrev(s): reversed in place
    r.add_on_demand(m, "_strrev", |c| {
        let p = c.arg(0);
        let n = (0..).take_while(|&i| c.host.read_u8(p + i).unwrap_or(0) != 0).count() as u32;
        let mut b = vec![0u8; n as usize];
        let _ = c.host.read(p, &mut b);
        b.reverse();
        let _ = c.host.write(p, &b);
        ok(p)
    });
    r.add_on_demand(m, "_wcsrev", |c| {
        let p = c.arg(0);
        let n = (0..).take_while(|&i| c.host.read_u16(p + 2 * i).unwrap_or(0) != 0).count() as u32;
        let mut u: Vec<u16> = (0..n).map(|i| c.host.read_u16(p + 2 * i).unwrap_or(0)).collect();
        u.reverse();
        for (i, v) in u.into_iter().enumerate() {
            let _ = c.host.write_u16(p + 2 * i as u32, v);
        }
        ok(p)
    });
    r.add_on_demand(m, "_wfopen", |c| {
        let (path, mode) = (c.wstr_arg(0), c.wstr_arg(1));
        let f = fopen(c, &path, &mode);
        c.trace(format!("({path:?}, {mode:?}) = {f:#x}"));
        ok(f)
    });
    r.add_on_demand(m, "_memicmp", |c| {
        let n = c.arg(2) as usize;
        let mut a = vec![0u8; n];
        let mut b = vec![0u8; n];
        let _ = c.host.read(c.arg(0), &mut a);
        let _ = c.host.read(c.arg(1), &mut b);
        let o = a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase());
        ok(o as i32 as u32)
    });
    r.add_on_demand(m, "fputs", |c| {
        let (p, f) = (c.arg(0), c.arg(1));
        let text = c.host.read_cstr(p).unwrap_or_default();
        match file_handle(c, f) {
            Some(h) => {
                super::kernel32::write_file(c, h, &text);
            }
            None => {
                // stdout/stderr: logged a line at a time
                for &ch in &text {
                    if ch == b'\n' {
                        let line = String::from_utf8_lossy(&std::mem::take(&mut c.p.crt_line)).into_owned();
                        c.p.log(format!("stdout: {line}"));
                    } else {
                        c.p.crt_line.push(ch);
                    }
                }
            }
        }
        ok(0)
    });
}
