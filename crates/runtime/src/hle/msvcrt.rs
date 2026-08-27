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

/// Reads successive cdecl variadic arguments.
pub struct VaList {
    pub p: u32,
}

impl VaList {
    fn u32(&mut self, c: &Ctx) -> u32 {
        let v = c.host.read_u32(self.p).unwrap_or(0);
        self.p += 4;
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
        let data = read_bytes(c, s, n);
        let _ = c.host.write(d, &data);
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
        let mut va = VaList { p: c.regs.esp + 12 };
        let mut out = format(c, &fmt, &mut va, false);
        let n = out.len() as u32;
        out.push(0);
        let _ = c.host.write(buf, &out);
        ok(n)
    });
    r.add(m, "_snprintf", |c| {
        let (buf, cap, fmt) = (c.arg(0), c.arg(1), c.host.read_cstr(c.arg(2)).unwrap_or_default());
        let mut va = VaList { p: c.regs.esp + 16 };
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
        let mut va = VaList { p: va };
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
        let mut va = VaList { p: c.regs.esp + 12 };
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
        let mut va = VaList { p: c.regs.esp + 8 };
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
        ("log10", f64::log10 as fn(f64) -> f64),
        ("log", f64::ln),
        ("exp", f64::exp),
        ("sqrt", f64::sqrt),
        ("sin", f64::sin),
        ("cos", f64::cos),
        ("tan", f64::tan),
        ("asin", f64::asin),
        ("acos", f64::acos),
        ("atan", f64::atan),
        ("floor", f64::floor),
        ("ceil", f64::ceil),
        ("fabs", f64::abs),
        ("sinh", f64::sinh),
        ("cosh", f64::cosh),
        ("tanh", f64::tanh),
    ] {
        r.add_math(m, name, f);
    }
    r.add(m, "pow", |c| {
        let (x, y) = (arg_f64(c, 0), arg_f64(c, 2));
        ret_f64(c, x.powf(y))
    });
    r.add(m, "atan2", |c| {
        let (y, x) = (arg_f64(c, 0), arg_f64(c, 2));
        ret_f64(c, y.atan2(x))
    });
    r.add(m, "fmod", |c| {
        let (x, y) = (arg_f64(c, 0), arg_f64(c, 2));
        ret_f64(c, x % y)
    });
    r.add(m, "ldexp", |c| {
        let (x, e) = (arg_f64(c, 0), c.arg_i32(2));
        ret_f64(c, x * 2f64.powi(e))
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
        ret_f64(c, x.powf(y))
    });
    r.add(m, "_CIatan2", |c| {
        let x = c.host.fpu_pop();
        let y = c.host.fpu_pop();
        ret_f64(c, y.atan2(x))
    });
    r.add(m, "_CIfmod", |c| {
        let y = c.host.fpu_pop();
        let x = c.host.fpu_pop();
        ret_f64(c, x % y)
    });
    for (name, f) in [
        ("_CIsqrt", f64::sqrt as fn(f64) -> f64),
        ("_CIsin", f64::sin),
        ("_CIcos", f64::cos),
        ("_CItan", f64::tan),
        ("_CIasin", f64::asin),
        ("_CIacos", f64::acos),
        ("_CIatan", f64::atan),
        ("_CIexp", f64::exp),
        ("_CIlog", f64::ln),
        ("_CIlog10", f64::log10),
        ("_CIsinh", f64::sinh),
        ("_CIcosh", f64::cosh),
        ("_CItanh", f64::tanh),
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
        let (m, e) = frexp(x * 2f64.powi(64));
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
