//! shlwapi's path and string helpers, on the guest's strings in place: ANSI ones as
//! bytes, wide ones as UTF-16 units (the API's name says which), as Wine's shlwapi
//! does them. Each replaces a stub that returned a constant (PathFindExtensionA gave
//! NULL, and MFC's CWinApp threw).

use super::{ApiFn, ApiResult, Ctx, Registry, Ret};

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

const BACKSLASH: u16 = b'\\' as u16;

fn wide(c: &Ctx) -> bool {
    c.name().ends_with('W')
}

/// The string at `a` (none for NULL), in units.
fn units(c: &Ctx, a: u32) -> Option<Vec<u16>> {
    if a == 0 {
        return None;
    }
    if wide(c) {
        let mut out = Vec::new();
        let mut p = a;
        while let Ok(u) = c.host.read_u16(p) {
            if u == 0 || out.len() > 32768 {
                break;
            }
            out.push(u);
            p += 2;
        }
        Some(out)
    } else {
        Some(c.host.read_cstr(a).unwrap_or_default().into_iter().map(u16::from).collect())
    }
}

fn put(c: &mut Ctx, a: u32, s: &[u16]) {
    if wide(c) {
        for (k, &u) in s.iter().chain([0u16].iter()).enumerate() {
            let _ = c.host.write_u16(a + 2 * k as u32, u);
        }
    } else {
        let b: Vec<u8> = s.iter().map(|&u| u as u8).collect();
        let _ = c.host.write_cstr(a, &b);
    }
}

/// The address of unit `i` of the string at `a`.
fn at(c: &Ctx, a: u32, i: usize) -> u32 {
    a + i as u32 * if wide(c) { 2 } else { 1 }
}

/// A path for the file system (ANSI as GetFileAttributesA reads it).
fn text(c: &Ctx, s: &[u16]) -> String {
    if wide(c) {
        String::from_utf16_lossy(s)
    } else {
        String::from_utf8_lossy(&s.iter().map(|&u| u as u8).collect::<Vec<u8>>()).into_owned()
    }
}

fn lower(u: u16) -> u16 {
    if (b'A' as u16..=b'Z' as u16).contains(&u) { u + 32 } else { u }
}

fn extension(s: &[u16]) -> usize {
    let mut last = None;
    for (i, &u) in s.iter().enumerate() {
        if u == BACKSLASH || u == b' ' as u16 {
            last = None;
        } else if u == b'.' as u16 {
            last = Some(i);
        }
    }
    last.unwrap_or(s.len())
}

fn file_name(s: &[u16]) -> usize {
    let mut last = 0;
    for i in 0..s.len() {
        let sep = s[i] == BACKSLASH || s[i] == b'/' as u16 || s[i] == b':' as u16;
        if sep && i + 1 < s.len() && s[i + 1] != BACKSLASH && s[i + 1] != b'/' as u16 {
            last = i + 1;
        }
    }
    last
}

fn is_unc(s: &[u16]) -> bool {
    s.len() >= 2 && s[0] == BACKSLASH && s[1] == BACKSLASH
}

fn is_relative(s: &[u16]) -> bool {
    !(s.first() == Some(&BACKSLASH) || s.get(1) == Some(&(b':' as u16)))
}

/// Where PathRemoveFileSpec cuts the path.
fn file_spec(s: &[u16]) -> usize {
    let mut p = 0;
    let mut spec = 0;
    for _ in 0..2 {
        if s.get(p) == Some(&BACKSLASH) {
            p += 1;
            spec = p;
        }
    }
    while p < s.len() {
        if s[p] == BACKSLASH {
            spec = p;
        } else if s[p] == b':' as u16 {
            p += 1;
            spec = p;
            if s.get(p) == Some(&BACKSLASH) {
                spec += 1;
            }
        }
        p += 1;
    }
    spec
}

/// `.` and `..` components resolved (PathCanonicalize); the root stays.
fn canonical(s: &[u16]) -> Vec<u16> {
    let root = if is_unc(s) {
        2
    } else if s.get(1) == Some(&(b':' as u16)) {
        if s.get(2) == Some(&BACKSLASH) { 3 } else { 2 }
    } else if s.first() == Some(&BACKSLASH) {
        1
    } else {
        0
    };
    let mut parts: Vec<&[u16]> = Vec::new();
    for part in s[root..].split(|&u| u == BACKSLASH) {
        if part == [b'.' as u16] {
            continue;
        }
        if part == [b'.' as u16, b'.' as u16] {
            parts.pop();
            continue;
        }
        parts.push(part);
    }
    let mut out = s[..root].to_vec();
    out.extend(parts.join(&BACKSLASH));
    out
}

fn combine(dir: Option<&[u16]>, file: Option<&[u16]>) -> Option<Vec<u16>> {
    let joined = match (dir, file) {
        (None, None) => return None,
        (Some(d), f) if f.is_none_or(|f| f.is_empty()) => d.to_vec(),
        (d, Some(f)) if d.is_none_or(|d| d.is_empty()) || !is_relative(f) => match d {
            Some(d) if !d.is_empty() && f.first() == Some(&BACKSLASH) && !is_unc(f) => {
                // the directory's root, then the file
                let mut t = d[..d.iter().position(|&u| u == BACKSLASH).map_or(d.len(), |p| p + 1)].to_vec();
                if t.last() != Some(&BACKSLASH) {
                    t.push(BACKSLASH);
                }
                t.extend_from_slice(&f[1..]);
                t
            }
            _ => f.to_vec(),
        },
        (Some(d), Some(f)) => {
            let mut t = d.to_vec();
            if t.last() != Some(&BACKSLASH) {
                t.push(BACKSLASH);
            }
            t.extend_from_slice(f);
            t
        }
        _ => return None,
    };
    Some(canonical(&joined))
}

/// `*` and `?`, case-insensitively.
fn matches(p: &[u16], n: &[u16]) -> bool {
    match (p.first(), n.first()) {
        (None, None) => true,
        (Some(&u), _) if u == b'*' as u16 => matches(&p[1..], n) || (!n.is_empty() && matches(p, &n[1..])),
        (Some(&u), Some(_)) if u == b'?' as u16 => matches(&p[1..], &n[1..]),
        (Some(&a), Some(&b)) if lower(a) == lower(b) => matches(&p[1..], &n[1..]),
        _ => false,
    }
}

fn add(r: &mut Registry, name: &str, f: ApiFn) {
    let dll = "shlwapi.dll";
    if r.lookup(dll, name).is_some() {
        r.add(dll, name, f);
    } else {
        r.add_on_demand(dll, name, f);
    }
}

pub fn register(r: &mut Registry) {
    for s in ["A", "W"] {
        add(r, &format!("PathFindExtension{s}"), |c| {
            let (a, s) = (c.arg(0), units(c, c.arg(0)));
            ok(1, s.map_or(0, |s| at(c, a, extension(&s))))
        });
        add(r, &format!("PathFindFileName{s}"), |c| {
            let (a, s) = (c.arg(0), units(c, c.arg(0)));
            ok(1, s.map_or(0, |s| at(c, a, file_name(&s))))
        });
        add(r, &format!("PathRemoveFileSpec{s}"), |c| {
            let Some(s) = units(c, c.arg(0)) else { return ok(1, 0) };
            let cut = file_spec(&s);
            if cut >= s.len() {
                return ok(1, 0);
            }
            put(c, c.arg(0), &s[..cut]);
            ok(1, 1)
        });
        add(r, &format!("PathStripPath{s}"), |c| {
            if let Some(s) = units(c, c.arg(0)) {
                let name = s[file_name(&s)..].to_vec();
                put(c, c.arg(0), &name);
            }
            ok(1, 0)
        });
        add(r, &format!("PathAddBackslash{s}"), |c| {
            let (a, Some(mut s)) = (c.arg(0), units(c, c.arg(0))) else { return ok(1, 0) };
            if !s.is_empty() && s.last() != Some(&BACKSLASH) {
                if s.len() >= 259 {
                    return ok(1, 0);
                }
                s.push(BACKSLASH);
                put(c, a, &s);
            }
            ok(1, at(c, a, s.len()))
        });
        add(r, &format!("PathAppend{s}"), |c| {
            let (Some(path), Some(more)) = (units(c, c.arg(0)), units(c, c.arg(1))) else { return ok(2, 0) };
            let more = if is_unc(&more) { &more[..] } else { &more[more.iter().take_while(|&&u| u == BACKSLASH).count()..] };
            match combine(Some(&path), Some(more)) {
                Some(t) => {
                    put(c, c.arg(0), &t);
                    ok(2, 1)
                }
                None => ok(2, 0),
            }
        });
        add(r, &format!("PathCombine{s}"), |c| {
            let dest = c.arg(0);
            if dest == 0 {
                return ok(3, 0);
            }
            let (dir, file) = (units(c, c.arg(1)), units(c, c.arg(2)));
            match combine(dir.as_deref(), file.as_deref()) {
                Some(t) => {
                    put(c, dest, &t);
                    ok(3, dest)
                }
                None => {
                    put(c, dest, &[]);
                    ok(3, 0)
                }
            }
        });
        add(r, &format!("PathFileExists{s}"), |c| {
            let Some(s) = units(c, c.arg(0)) else { return ok(1, 0) };
            let key = c.p.vpath(&text(c, &s));
            ok(1, c.p.fs.stat(&key).is_some() as u32)
        });
        add(r, &format!("PathIsDirectory{s}"), |c| {
            let Some(s) = units(c, c.arg(0)) else { return ok(1, 0) };
            let key = c.p.vpath(&text(c, &s));
            ok(1, if c.p.fs.stat(&key).is_some_and(|m| m.is_dir) { 0x10 } else { 0 })
        });
        add(r, &format!("PathIsRelative{s}"), |c| ok(1, units(c, c.arg(0)).is_none_or(|s| s.is_empty() || is_relative(&s)) as u32));
        add(r, &format!("PathIsUNC{s}"), |c| ok(1, units(c, c.arg(0)).is_some_and(|s| is_unc(&s)) as u32));
        add(r, &format!("PathMatchSpec{s}"), |c| {
            let (Some(name), Some(spec)) = (units(c, c.arg(0)), units(c, c.arg(1))) else { return ok(2, 0) };
            let any = spec.split(|&u| u == b';' as u16).any(|p| {
                let p: Vec<u16> = p.iter().copied().skip_while(|&u| u == b' ' as u16).collect();
                p == [b'*' as u16, b'.' as u16, b'*' as u16] || matches(&p, &name)
            });
            ok(2, any as u32)
        });
        add(r, &format!("PathQuoteSpaces{s}"), |c| {
            let Some(s) = units(c, c.arg(0)) else { return ok(1, 0) };
            if !s.contains(&(b' ' as u16)) || s.len() + 2 >= 260 {
                return ok(1, 0);
            }
            let q = b'"' as u16;
            let t: Vec<u16> = [q].into_iter().chain(s).chain([q]).collect();
            put(c, c.arg(0), &t);
            ok(1, 1)
        });
        add(r, &format!("PathUnquoteSpaces{s}"), |c| {
            if let Some(s) = units(c, c.arg(0)) {
                let q = b'"' as u16;
                if s.len() >= 2 && s[0] == q && s[s.len() - 1] == q {
                    put(c, c.arg(0), &s[1..s.len() - 1]);
                }
            }
            ok(1, 0)
        });
        add(r, &format!("StrCmpNI{s}"), |c| {
            let (a, b, n) = (units(c, c.arg(0)).unwrap_or_default(), units(c, c.arg(1)).unwrap_or_default(), c.arg(2) as usize);
            let key = |s: &[u16]| s.iter().take(n).map(|&u| lower(u)).collect::<Vec<u16>>();
            ok(3, key(&a).cmp(&key(&b)) as i32 as u32)
        });
        add(r, &format!("StrStrI{s}"), |c| {
            let (Some(hay), Some(needle)) = (units(c, c.arg(0)), units(c, c.arg(1))) else { return ok(2, 0) };
            let lo = |s: &[u16]| s.iter().map(|&u| lower(u)).collect::<Vec<u16>>();
            let (h, n) = (lo(&hay), lo(&needle));
            let found = if n.is_empty() { None } else { h.windows(n.len()).position(|w| w == n.as_slice()) };
            ok(2, found.map_or(0, |i| at(c, c.arg(0), i)))
        });
        add(r, &format!("StrChr{s}"), |c| {
            let Some(s) = units(c, c.arg(0)) else { return ok(2, 0) };
            let ch = c.arg(1) as u16;
            ok(2, s.iter().position(|&u| u == ch).map_or(0, |i| at(c, c.arg(0), i)))
        });
        add(r, &format!("StrRChr{s}"), |c| {
            let (start, end) = (c.arg(0), c.arg(1));
            let Some(s) = units(c, start) else { return ok(3, 0) };
            let w = if wide(c) { 2 } else { 1 };
            let n = if end == 0 { s.len() } else { (end.saturating_sub(start) / w) as usize }.min(s.len());
            let ch = c.arg(2) as u16;
            ok(3, s[..n].iter().rposition(|&u| u == ch).map_or(0, |i| at(c, start, i)))
        });
        add(r, &format!("StrToInt{s}"), |c| {
            let s = units(c, c.arg(0)).unwrap_or_default();
            let neg = s.first() == Some(&(b'-' as u16));
            let digits = s.iter().skip(neg as usize).take_while(|&&u| (b'0' as u16..=b'9' as u16).contains(&u));
            let v = digits.fold(0i32, |v, &u| v.wrapping_mul(10).wrapping_add((u - b'0' as u16) as i32));
            ok(1, if neg { v.wrapping_neg() } else { v } as u32)
        });
        add(r, &format!("StrDup{s}"), |c| {
            let Some(s) = units(c, c.arg(0)) else { return ok(1, 0) };
            let w = if wide(c) { 2 } else { 1 };
            let heap = c.p.process_heap;
            let g: &mut dyn crate::mem::Guest = &mut *c.host;
            let a = c.p.heap_alloc(g, heap, (s.len() as u32 + 1) * w, false);
            if a != 0 {
                put(c, a, &s);
            }
            ok(1, a)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn paths() {
        assert_eq!(extension(&u("C:\\game\\FileManager.dll")), 19);
        assert_eq!(extension(&u("C:\\a.b\\noext")), 12);
        assert_eq!(file_name(&u("C:\\game\\x.exe")), 8);
        assert_eq!(file_name(&u("x.exe")), 0);
        assert_eq!(file_spec(&u("C:\\game\\x.exe")), 7);
        assert_eq!(file_spec(&u("C:\\x.exe")), 3);
        assert_eq!(combine(Some(&u("C:\\game")), Some(&u("data\\..\\save.ini"))), Some(u("C:\\game\\save.ini")));
        assert_eq!(combine(Some(&u("C:\\game\\sub")), Some(&u("\\top.txt"))), Some(u("C:\\top.txt")));
        assert_eq!(combine(Some(&u("C:\\game")), Some(&u("D:\\other"))), Some(u("D:\\other")));
        assert!(matches(&u("*.ogg"), &u("Music.OGG")));
        assert!(!matches(&u("*.ogg"), &u("music.wav")));
    }
}
