//! The debug switch (patcher.rs): a GameMaker: Studio game keeps its debug flags in
//! variables (`debugMode`, ...) that its start-up code sets to false; the patch makes that
//! assignment set true.
//!
//! VM games: the variable's assignment in the bytecode (`pushi 0`, `pop <var>`; found
//! through the variable's chain of references in VARI) gets `pushi 1`. patcher.rs patched
//! the instruction before the variable's *first* reference, which for `debugNoDeath` is
//! usually a read whose preceding instruction is a branch: only an assignment of 0 is
//! changed here.
//!
//! YYC games: patcher.rs's search, unchanged (the variable's slot in the global table,
//! then the code that stores 0.0 into it).

/// Variables patched in VM games.
pub const VM_NAMES: &[&str] = &["debugMode", "debug_mode", "debug", "debugUse", "practiceMode", "isDebug"];
/// With the no-death option as well.
pub const VM_NO_DEATH: &str = "debugNoDeath";
/// Variables looked for in YYC games.
pub const YYC_NAMES: &[&str] = &["debugMode", "debug_mode", "debug", "debugUse", "practiceMode", "isDebug", "debugState"];

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// The chunks of a FORM (a data.win): name -> (data offset, size).
pub fn chunks(form: &[u8]) -> Option<Vec<([u8; 4], usize, usize)>> {
    if form.get(0..4)? != b"FORM" {
        return None;
    }
    let end = (8 + u32_at(form, 4)? as usize).min(form.len());
    let mut out = Vec::new();
    let mut o = 8;
    while o + 8 <= end {
        let name: [u8; 4] = form[o..o + 4].try_into().unwrap();
        let size = u32_at(form, o + 4)? as usize;
        out.push((name, o + 8, size));
        o += 8 + size;
    }
    Some(out)
}

fn c_str(d: &[u8], at: usize) -> String {
    let s = d.get(at..).unwrap_or(&[]);
    let n = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..n]).into_owned()
}

/// Sets the debug variables `names` to true in the FORM `form` (a whole data.win, or the
/// one inside an exe); returns what it did, one line per variable found.
pub fn patch_vm(form: &mut [u8], names: &[&str]) -> Result<Vec<String>, String> {
    let ch = chunks(form).ok_or("not a GameMaker data file (no FORM)")?;
    let find = |n: &[u8; 4]| ch.iter().find(|c| &c.0 == n).map(|c| (c.1, c.2));
    let (gen8, _) = find(b"GEN8").ok_or("no GEN8 chunk")?;
    let bytecode = *form.get(gen8 + 1).ok_or("GEN8 too short")?;
    let (vari, vari_size) = find(b"VARI").ok_or("no VARI chunk")?;
    // bytecode 15 on: a 12-byte header, then [name, instance type, id, occurrences, first];
    // before: [name, occurrences, first]
    let (header, entry) = if bytecode >= 15 { (12, 20) } else { (0, 12) };
    let (push_op, pop_op) = if bytecode >= 15 { (0x84u8, 0x45u8) } else { (0xC0, 0x41) };
    let mut log = Vec::new();
    let mut o = vari + header;
    while o + entry <= vari + vari_size {
        let name = c_str(form, u32_at(form, o).unwrap_or(0) as usize);
        if !names.contains(&name.as_str()) {
            o += entry;
            continue;
        }
        let occurrences = u32_at(form, o + entry - 8).unwrap_or(0);
        let mut at = u32_at(form, o + entry - 4).unwrap_or(0) as usize;
        let mut done = false;
        for _ in 0..occurrences {
            let (Some(prev), Some(cur)) = (at.checked_sub(4).and_then(|p| u32_at(form, p)), u32_at(form, at)) else { break };
            // pushi.e (an int16 constant) right before a pop into the variable
            if (prev >> 24) as u8 == push_op && (prev >> 16) & 0xF == 0xF && (cur >> 24) as u8 == pop_op {
                let value = prev & 0xFFFF;
                if value == 0 {
                    form[at - 4] = 1;
                    form[at - 3] = 0;
                    log.push(format!("{name} = true (at {:#x})", at - 4));
                } else {
                    log.push(format!("{name} is already set to {value}"));
                }
                done = true;
                break;
            }
            let next = (u32_at(form, at + 4).unwrap_or(0) & 0x07FF_FFFF) as usize;
            if next == 0 {
                break;
            }
            at += next;
        }
        if !done {
            log.push(format!("{name}: no assignment of a constant found, left alone"));
        }
        o += entry;
    }
    if log.is_empty() {
        return Err("no debug variables in this game".into());
    }
    Ok(log)
}

/// patcher.rs's file offsets of false positives (state-machine counters, not variable
/// slots), skipped in both search modes.
const EXCLUDE_OFFSETS: &[usize] = &[0x25E14, 0x35C2];

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// patcher.rs's YYC patch of the exe: the debug variables' slots in the runner's variable
/// table, then the code that stores 0.0 into them stores 1.0.
pub fn patch_yyc(exe: &mut [u8]) -> Result<Vec<String>, String> {
    if exe.len() < 1024 || &exe[0..2] != b"MZ" {
        return Err("not an executable".into());
    }
    let pe = u32_at(exe, 0x3C).unwrap_or(0) as usize;
    if exe.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("not a PE executable".into());
    }
    let sections = u16::from_le_bytes([exe[pe + 6], exe[pe + 7]]) as usize;
    let opt = u16::from_le_bytes([exe[pe + 20], exe[pe + 21]]) as usize;
    let image_base = u32_at(exe, pe + 52).unwrap_or(0) as usize;
    let table = pe + 24 + opt;
    let mut secs = Vec::new();
    for i in 0..sections {
        let s = table + i * 40;
        let (Some(va), Some(size), Some(raw)) = (u32_at(exe, s + 12), u32_at(exe, s + 16), u32_at(exe, s + 20)) else { break };
        secs.push((image_base + va as usize, raw as usize, size as usize));
    }
    let to_va = |off: usize| secs.iter().find(|&&(_, raw, size)| off >= raw && off < raw + size).map(|&(va, raw, _)| va + off - raw);

    struct Var {
        name: &'static str,
        id: u32,
        slot: u32,
    }
    let mut vars: Vec<Var> = Vec::new();
    let mut log = Vec::new();
    for &name in YYC_NAMES {
        let mut needle = name.as_bytes().to_vec();
        needle.push(0);
        let mut pos = 0;
        let mut found = false;
        while let Some(i) = find(&exe[pos..], &needle) {
            let at = pos + i;
            if let Some(va) = to_va(at) {
                let va_bytes = (va as u32).to_le_bytes();
                let mut t = 0;
                while let Some(j) = find(&exe[t..], &va_bytes) {
                    let r = t + j;
                    if let (Some(np), Some(id)) = (u32_at(exe, r), u32_at(exe, r + 4)) {
                        if np as usize == va && id < 10000 {
                            if !vars.iter().any(|v| v.id == id) {
                                vars.push(Var { name, id, slot: id * 16 });
                            }
                            found = true;
                            break;
                        }
                    }
                    t = r + 4;
                }
            }
            if found {
                break;
            }
            pos = at + needle.len();
        }
    }
    if vars.is_empty() {
        return Err("no debug variables in this game".into());
    }

    let limit = 0x50000.min(exe.len());
    let mut patched: Vec<usize> = Vec::new();
    let set_one = |exe: &mut [u8], i: usize| exe[i + 3..i + 7].copy_from_slice(&[0x00, 0x00, 0xF0, 0x3F]);
    // A: `lea reg, [x + slot]`, then `mov dword [reg+4], 0; mov dword [reg], 0` within 2 KB
    for v in &vars {
        let slot = v.slot.to_le_bytes();
        for pos in 0..limit.saturating_sub(6) {
            if exe[pos] != 0x8D || exe[pos + 1] & 0xC0 != 0x80 || exe[pos + 2..pos + 6] != slot {
                continue;
            }
            let reg = (exe[pos + 1] >> 3) & 7;
            let end = (pos + 2048).min(exe.len());
            for i in pos..end.saturating_sub(13) {
                if exe[i] == 0xC7 && exe[i + 1] == 0x40 + reg && exe[i + 2] == 0x04 && exe[i + 3..i + 7] == [0; 4] && exe[i + 7] == 0xC7 && exe[i + 8] == reg && exe[i + 9..i + 13] == [0; 4] {
                    if !patched.contains(&i) && !EXCLUDE_OFFSETS.contains(&i) {
                        set_one(exe, i);
                        patched.push(i);
                        log.push(format!("{} = true (at {i:#x})", v.name));
                        break;
                    }
                }
            }
        }
    }
    // B: the variable's id pushed as a parameter, then the same stores within 256 bytes,
    // only near a hit of A (the same init function); at most two per variable
    let anchors = patched.clone();
    for v in &vars {
        let id = v.id.to_le_bytes();
        let before = patched.len();
        let mut pos = 0;
        while pos + 4 <= limit {
            let Some(rel) = find(&exe[pos..limit], &id) else { break };
            let at = pos + rel;
            let end = (at + 256).min(exe.len());
            for i in at..end.saturating_sub(13) {
                if exe[i] == 0xC7 && (0x40..=0x47).contains(&exe[i + 1]) && exe[i + 2] == 0x04 && exe[i + 3..i + 7] == [0; 4] && exe[i + 7] == 0xC7 && exe[i + 8] <= 7 && exe[i + 9..i + 13] == [0; 4] {
                    if patched.contains(&i) || EXCLUDE_OFFSETS.contains(&i) {
                        continue;
                    }
                    if !anchors.iter().any(|&a| a.abs_diff(i) <= 2048) {
                        continue;
                    }
                    set_one(exe, i);
                    patched.push(i);
                    log.push(format!("{} = true (at {i:#x})", v.name));
                    break;
                }
            }
            if patched.len() - before >= 2 {
                break;
            }
            pos = at + 4;
        }
    }
    // C: the address kept in a stack slot (`lea r, [x + slot]; mov [esp + d], r`), and the
    // first store of 0.0 through it (`mov r2, [esp + d]`, then the stores via r2) further
    // on in the function; one per variable (GMS 1.4's YYC sets globals this way)
    for v in &vars {
        if log.iter().any(|l| l.starts_with(v.name) && l[v.name.len()..].starts_with(" = ")) {
            continue;
        }
        let slot = v.slot.to_le_bytes();
        let text_end = secs.first().map_or(exe.len(), |&(_, raw, size)| (raw + size).min(exe.len()));
        'sites: for pos in 0..text_end.saturating_sub(16) {
            if exe[pos] != 0x8D || exe[pos + 1] & 0xC0 != 0x80 || exe[pos + 1] & 7 == 4 || exe[pos + 2..pos + 6] != slot {
                continue;
            }
            let reg = (exe[pos + 1] >> 3) & 7;
            // mov [esp + disp8], reg
            let Some(disp) = (exe[pos + 6] == 0x89 && exe[pos + 7] == 0x44 | reg << 3 && exe[pos + 8] == 0x24).then(|| exe[pos + 9]) else { continue };
            let end = (pos + 0x4000).min(text_end);
            for j in pos + 10..end.saturating_sub(20) {
                // mov r2, [esp + disp8]
                if exe[j] != 0x8B || exe[j + 1] & 0xC7 != 0x44 || exe[j + 2] != 0x24 || exe[j + 3] != disp {
                    continue;
                }
                let r2 = (exe[j + 1] >> 3) & 7;
                if r2 == 4 || r2 == 5 {
                    continue;
                }
                for i in j + 4..(j + 64).min(end.saturating_sub(13)) {
                    if exe[i] == 0xC7 && exe[i + 1] == 0x40 + r2 && exe[i + 2] == 0x04 && exe[i + 3..i + 7] == [0; 4] && exe[i + 7] == 0xC7 && exe[i + 8] == r2 && exe[i + 9..i + 13] == [0; 4] {
                        if patched.contains(&i) || EXCLUDE_OFFSETS.contains(&i) {
                            continue;
                        }
                        set_one(exe, i);
                        patched.push(i);
                        log.push(format!("{} = true (at {i:#x})", v.name));
                        break 'sites;
                    }
                }
                // the slot was reloaded for something else (a read): keep looking
            }
        }
    }
    if patched.is_empty() {
        return Err(format!(
            "found {} but not where the game sets it",
            vars.iter().map(|v| v.name).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(log)
}
