//! INI files through the Windows profile API (GetPrivateProfileString and friends),
//! read from and written to the VFS like any file. Delphi's TIniFile (GameMaker 8
//! extensions, and programs built with it) and many DLLs keep their settings this way.
//! A bare file name means the Windows directory, as on Windows. Files are ANSI: the A
//! functions work on their bytes, the W functions convert through the ANSI code page.

use super::{ApiResult, Ctx, Registry, Ret};

const ACP: u32 = 1252;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

/// A string argument as bytes (NULL = None).
fn arg(c: &Ctx, i: u32, wide: bool) -> Option<Vec<u8>> {
    let p = c.arg(i);
    if p == 0 {
        return None;
    }
    Some(if wide { super::kernel32::encode_cp(ACP, &c.host.read_wstr(p).unwrap_or_default()) } else { c.host.read_cstr(p).unwrap_or_default() })
}

fn ini_key(c: &Ctx, name: &[u8]) -> String {
    let name = String::from_utf8_lossy(name).into_owned();
    let guest = if name.contains('\\') || name.contains('/') { name } else { format!("C:\\Windows\\{name}") };
    c.p.vpath(&guest)
}

fn load(c: &Ctx, key: &str) -> Vec<Vec<u8>> {
    let bytes = c.p.fs.read_all(key).unwrap_or_default();
    bytes.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec()).collect()
}

fn save(c: &mut Ctx, key: &str, lines: &[Vec<u8>]) -> bool {
    let mut out = Vec::new();
    for l in lines {
        out.extend_from_slice(l);
        out.extend_from_slice(b"\r\n");
    }
    // drop the empty line the split leaves after a final line break
    while out.ends_with(b"\r\n\r\n") {
        out.truncate(out.len() - 2);
    }
    if !c.p.fs.create(key) {
        if let Some((dir, _)) = key.rsplit_once('\\') {
            c.p.fs.mkdir(dir);
        }
        if !c.p.fs.create(key) {
            return false;
        }
    }
    c.p.fs.set_len(key, 0) && c.p.fs.write_at(key, 0, &out)
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !b.is_ascii_whitespace()).map(|e| e + 1).unwrap_or(start);
    &s[start..end]
}

fn section_of(line: &[u8]) -> Option<&[u8]> {
    let t = trim(line);
    if t.first() == Some(&b'[') {
        let end = t.iter().position(|&b| b == b']').unwrap_or(t.len());
        Some(trim(&t[1..end]))
    } else {
        None
    }
}

fn key_value(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let t = trim(line);
    if t.is_empty() || t[0] == b';' || t[0] == b'[' {
        return None;
    }
    match t.iter().position(|&b| b == b'=') {
        Some(i) => Some((trim(&t[..i]), trim(&t[i + 1..]))),
        None => Some((t, &[][..])),
    }
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Line ranges of a section's entries (after its header).
fn section_range(lines: &[Vec<u8>], section: &[u8]) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| section_of(l).is_some_and(|s| same(s, section)))?;
    let end = lines[start + 1..].iter().position(|l| section_of(l).is_some()).map(|e| start + 1 + e).unwrap_or(lines.len());
    Some((start, end))
}

fn unquote(v: &[u8]) -> &[u8] {
    if v.len() >= 2 && (v[0] == b'"' || v[0] == b'\'') && v[v.len() - 1] == v[0] { &v[1..v.len() - 1] } else { v }
}

fn lookup(lines: &[Vec<u8>], section: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let (s, e) = section_range(lines, section)?;
    lines[s + 1..e].iter().filter_map(|l| key_value(l)).find(|(k, _)| same(k, key)).map(|(_, v)| unquote(v).to_vec())
}

/// Copies one string into a buffer of `size` characters; returns what Windows does
/// (the characters copied, without the terminator).
fn out_string(c: &mut Ctx, buf: u32, size: u32, s: &[u8], wide: bool) -> u32 {
    if buf == 0 || size == 0 {
        return 0;
    }
    let units: Vec<u16> = if wide { super::kernel32::decode_cp(ACP, s).encode_utf16().collect() } else { s.iter().map(|&b| b as u16).collect() };
    let n = units.len().min(size as usize - 1);
    write_units(c, buf, &units[..n], wide);
    write_units(c, buf + n as u32 * if wide { 2 } else { 1 }, &[0], wide);
    n as u32
}

/// Copies strings as a double-terminated list; truncated lists end in two terminators
/// and report `size - 2`, as on Windows.
fn out_list(c: &mut Ctx, buf: u32, size: u32, items: &[Vec<u8>], wide: bool) -> u32 {
    if buf == 0 || size < 2 {
        return 0;
    }
    let mut units: Vec<u16> = Vec::new();
    for it in items {
        if wide {
            units.extend(super::kernel32::decode_cp(ACP, it).encode_utf16());
        } else {
            units.extend(it.iter().map(|&b| b as u16));
        }
        units.push(0);
    }
    let cap = size as usize;
    let (n, ret) = if units.len() + 1 <= cap { (units.len(), units.len() as u32) } else { (cap - 2, cap as u32 - 2) };
    let mut body = units[..n].to_vec();
    body.push(0);
    if n == cap - 2 {
        body.push(0);
    }
    write_units(c, buf, &body, wide);
    if items.is_empty() {
        write_units(c, buf, &[0, 0], wide);
        return 0;
    }
    ret
}

fn write_units(c: &mut Ctx, at: u32, units: &[u16], wide: bool) {
    if wide {
        let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let _ = c.host.write(at, &bytes);
    } else {
        let bytes: Vec<u8> = units.iter().map(|&u| u as u8).collect();
        let _ = c.host.write(at, &bytes);
    }
}

fn get_string(c: &mut Ctx, wide: bool, file: Option<Vec<u8>>, base: u32) -> u32 {
    let (section, key, default) = (arg(c, base, wide), arg(c, base + 1, wide), arg(c, base + 2, wide).unwrap_or_default());
    let (buf, size) = (c.arg(base + 3), c.arg(base + 4));
    let file = file.unwrap_or_default();
    let lines = load(c, &ini_key(c, &file));
    match (section, key) {
        (None, _) => {
            let names: Vec<Vec<u8>> = lines.iter().filter_map(|l| section_of(l).map(|s| s.to_vec())).collect();
            out_list(c, buf, size, &names, wide)
        }
        (Some(s), None) => {
            let keys: Vec<Vec<u8>> = match section_range(&lines, &s) {
                Some((a, b)) => lines[a + 1..b].iter().filter_map(|l| key_value(l).map(|(k, _)| k.to_vec())).collect(),
                None => Vec::new(),
            };
            out_list(c, buf, size, &keys, wide)
        }
        (Some(s), Some(k)) => {
            let v = lookup(&lines, &s, &k).unwrap_or_else(|| trim(&default).to_vec());
            out_string(c, buf, size, &v, wide)
        }
    }
}

fn parse_int(v: &[u8], default: u32) -> u32 {
    let t = trim(v);
    let (neg, digits) = match t.first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let n: Vec<u8> = digits.iter().copied().take_while(|b| b.is_ascii_digit()).collect();
    if n.is_empty() {
        return if t.is_empty() { default } else { 0 };
    }
    let v: i64 = std::str::from_utf8(&n).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    (if neg { -v } else { v }) as i32 as u32
}

fn write_string(c: &mut Ctx, wide: bool, section: Option<Vec<u8>>, key: Option<Vec<u8>>, value: Option<Vec<u8>>, file: &[u8]) -> bool {
    let Some(section) = section else { return false };
    let path = ini_key(c, file);
    let mut lines = load(c, &path);
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let range = section_range(&lines, &section);
    match (key, value) {
        (None, _) => {
            // delete the section
            if let Some((a, b)) = range {
                lines.drain(a..b);
            }
        }
        (Some(k), None) => {
            if let Some((a, b)) = range {
                if let Some(i) = (a + 1..b).find(|&i| key_value(&lines[i]).is_some_and(|(kk, _)| same(kk, &k))) {
                    lines.remove(i);
                }
            }
        }
        (Some(k), Some(v)) => {
            let mut entry = k.clone();
            entry.push(b'=');
            entry.extend_from_slice(&v);
            match range {
                Some((a, b)) => match (a + 1..b).find(|&i| key_value(&lines[i]).is_some_and(|(kk, _)| same(kk, &k))) {
                    Some(i) => lines[i] = entry,
                    None => {
                        // after the section's last non-empty line
                        let mut at = b;
                        while at > a + 1 && trim(&lines[at - 1]).is_empty() {
                            at -= 1;
                        }
                        lines.insert(at, entry);
                    }
                },
                None => {
                    let mut header = b"[".to_vec();
                    header.extend_from_slice(&section);
                    header.push(b']');
                    lines.push(header);
                    lines.push(entry);
                }
            }
        }
    }
    let _ = wide;
    save(c, &path, &lines)
}

pub fn register(r: &mut Registry) {
    let k = "kernel32.dll";
    r.add(k, "GetPrivateProfileStringA", |c| {
        let f = arg(c, 5, false);
        let n = get_string(c, false, f, 0);
        ok(6, n)
    });
    r.add(k, "GetPrivateProfileStringW", |c| {
        let f = arg(c, 5, true);
        let n = get_string(c, true, f, 0);
        ok(6, n)
    });
    r.add(k, "GetProfileStringA", |c| {
        let n = get_string(c, false, Some(b"win.ini".to_vec()), 0);
        ok(5, n)
    });
    r.add(k, "GetProfileStringW", |c| {
        let n = get_string(c, true, Some(b"win.ini".to_vec()), 0);
        ok(5, n)
    });
    fn get_int(c: &mut Ctx, wide: bool, file: Vec<u8>, nargs: u32) -> ApiResult {
        let (s, key, default) = (arg(c, 0, wide).unwrap_or_default(), arg(c, 1, wide).unwrap_or_default(), c.arg(2));
        let lines = load(c, &ini_key(c, &file));
        let v = lookup(&lines, &s, &key).map(|v| parse_int(&v, default)).unwrap_or(default);
        ok(nargs, v)
    }
    r.add(k, "GetPrivateProfileIntA", |c| {
        let f = arg(c, 3, false).unwrap_or_default();
        get_int(c, false, f, 4)
    });
    r.add(k, "GetPrivateProfileIntW", |c| {
        let f = arg(c, 3, true).unwrap_or_default();
        get_int(c, true, f, 4)
    });
    r.add(k, "GetProfileIntA", |c| get_int(c, false, b"win.ini".to_vec(), 3));
    r.add(k, "GetProfileIntW", |c| get_int(c, true, b"win.ini".to_vec(), 3));
    fn write(c: &mut Ctx, wide: bool, file: Vec<u8>, nargs: u32) -> ApiResult {
        let (s, key, v) = (arg(c, 0, wide), arg(c, 1, wide), arg(c, 2, wide));
        let done = write_string(c, wide, s, key, v, &file);
        ok(nargs, done as u32)
    }
    r.add(k, "WritePrivateProfileStringA", |c| {
        let f = arg(c, 3, false).unwrap_or_default();
        write(c, false, f, 4)
    });
    r.add(k, "WritePrivateProfileStringW", |c| {
        let f = arg(c, 3, true).unwrap_or_default();
        write(c, true, f, 4)
    });
    r.add(k, "WriteProfileStringA", |c| write(c, false, b"win.ini".to_vec(), 3));
    r.add(k, "WriteProfileStringW", |c| write(c, true, b"win.ini".to_vec(), 3));
    fn section(c: &mut Ctx, wide: bool) -> ApiResult {
        let s = arg(c, 0, wide).unwrap_or_default();
        let (buf, size) = (c.arg(1), c.arg(2));
        let file = arg(c, 3, wide).unwrap_or_default();
        let lines = load(c, &ini_key(c, &file));
        let entries: Vec<Vec<u8>> = match section_range(&lines, &s) {
            Some((a, b)) => lines[a + 1..b].iter().map(|l| trim(l).to_vec()).filter(|l| !l.is_empty() && l[0] != b';').collect(),
            None => Vec::new(),
        };
        let n = out_list(c, buf, size, &entries, wide);
        ok(4, n)
    }
    r.add(k, "GetPrivateProfileSectionA", |c| section(c, false));
    r.add(k, "GetPrivateProfileSectionW", |c| section(c, true));
    fn names(c: &mut Ctx, wide: bool) -> ApiResult {
        let (buf, size) = (c.arg(0), c.arg(1));
        let file = arg(c, 2, wide).unwrap_or_default();
        let lines = load(c, &ini_key(c, &file));
        let names: Vec<Vec<u8>> = lines.iter().filter_map(|l| section_of(l).map(|s| s.to_vec())).collect();
        let n = out_list(c, buf, size, &names, wide);
        ok(3, n)
    }
    r.add(k, "GetPrivateProfileSectionNamesA", |c| names(c, false));
    r.add(k, "GetPrivateProfileSectionNamesW", |c| names(c, true));
    fn write_section(c: &mut Ctx, wide: bool) -> ApiResult {
        // replaces the section with the "key=value" strings of a double-terminated list
        let s = arg(c, 0, wide);
        let file = arg(c, 2, wide).unwrap_or_default();
        let p = c.arg(1);
        let mut entries = Vec::new();
        let mut at = p;
        loop {
            let item = if wide {
                let w = c.host.read_wstr(at).unwrap_or_default();
                at += 2 * (w.encode_utf16().count() as u32 + 1);
                super::kernel32::encode_cp(ACP, &w)
            } else {
                let b = c.host.read_cstr(at).unwrap_or_default();
                at += b.len() as u32 + 1;
                b
            };
            if item.is_empty() {
                break;
            }
            entries.push(item);
        }
        let done = write_string(c, wide, s.clone(), None, None, &file);
        for e in entries {
            let (kk, vv) = match e.iter().position(|&b| b == b'=') {
                Some(i) => (e[..i].to_vec(), e[i + 1..].to_vec()),
                None => (e.clone(), Vec::new()),
            };
            write_string(c, wide, s.clone(), Some(kk), Some(vv), &file);
        }
        ok(3, done as u32)
    }
    r.add(k, "WritePrivateProfileSectionA", |c| write_section(c, false));
    r.add(k, "WritePrivateProfileSectionW", |c| write_section(c, true));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<Vec<u8>> {
        s.split('\n').map(|l| l.as_bytes().to_vec()).collect()
    }

    #[test]
    fn looks_up_case_insensitively_and_unquotes() {
        let l = lines("[Main]\n Volume = 7 \nName=\"Kid\"\n[Other]\nVolume=3");
        assert_eq!(lookup(&l, b"main", b"VOLUME"), Some(b"7".to_vec()));
        assert_eq!(lookup(&l, b"Main", b"name"), Some(b"Kid".to_vec()));
        assert_eq!(lookup(&l, b"Other", b"volume"), Some(b"3".to_vec()));
        assert_eq!(lookup(&l, b"Missing", b"volume"), None);
        assert_eq!(parse_int(b" -12x", 5), (-12i32) as u32);
        assert_eq!(parse_int(b"", 5), 5);
    }
}
