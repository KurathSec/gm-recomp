//! GameMaker: Studio 1.4 game data (a data.win, bytecode 14 to 16), changed for the mods.
//!
//! The runner reads the file through absolute pointers, so the file only grows at its end.
//! A change that keeps a structure's size is made where it is. Anything new goes into a
//! pool after the old end of the file (trailing data of the chunk that ends there, which
//! the runner does not read), and a chunk whose list grows is copied to the end with the
//! longer list, its old bytes becoming trailing data of the chunk before it. The old
//! entries stay where they are, pointed to from the copies; VARI and FUNC, which the
//! runner reads as arrays, are rewritten whole in theirs.
//!
//! Runners 1.4.1598 to 1.4.9999 load such a file as long as the chunk before a moved one
//! is a list (not VARI or FUNC, whose size gives their count in bytecode 14, nor an empty
//! chunk), and SCPT stays before CODE. They also hand each texture page's PNG to D3DX with
//! TXTR's size as its length, so a copy of TXTR is as long as the longest PNG.
//!
//! New code refers to variables and functions as compiled code does: each reference is a
//! link in its VARI or FUNC entry's chain (the entry has the first occurrence's address,
//! each operand the distance to the next, the last one the name's string index), and new
//! references are linked onto the ends of the chains when the file is written. The runner
//! runs one code action per event, so code for an event that has some is appended to it
//! (its bytecode copied, the copy's references linked on like new ones).

use crate::gm8::Image;
use crate::gms_asm::{self as asm, Ins, Op, Ref, ty};
use crate::gms_builtins;
use std::collections::HashMap;

type Result<T> = std::result::Result<T, String>;

fn get(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or_else(|| format!("the data file ends before {o:#x}"))
}

fn c_str(d: &[u8], at: usize) -> &[u8] {
    let s = d.get(at..).unwrap_or(&[]);
    &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
}

fn words(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[derive(Clone, Copy, Debug)]
struct Chunk {
    name: [u8; 4],
    /// the header's offset
    at: usize,
    size: usize,
}

impl Chunk {
    fn data(&self) -> usize {
        self.at + 8
    }
    fn end(&self) -> usize {
        self.at + 8 + self.size
    }
}

/// A VARI or FUNC entry.
#[derive(Clone, Debug)]
struct Entry {
    /// the name's characters
    name: u32,
    /// instance type and id (VARI, bytecode 15 on)
    inst: i32,
    id: i32,
    occurrences: u32,
    first: u32,
    /// new references (their instructions' addresses), linked on when written
    new: Vec<u32>,
    /// the name's string index, for an entry added here
    name_index: Option<u32>,
}

/// A texture page added here, packed in rows.
#[derive(Default)]
struct Page {
    images: Vec<(u32, u32, Image)>,
    row_y: u32,
    row_h: u32,
    x: u32,
}

const PAGE: u32 = 2048;
/// between images on a page (filtering does not bleed)
const GAP: u32 = 2;

/// Object event types, by number (their code entries' names).
const EVENTS: &[&str] = &["Create", "Destroy", "Alarm", "Step", "Collision", "Keyboard", "Mouse", "Other", "Draw", "KeyPress", "KeyRelease", "Trigger"];

/// Asset chunks, in the order names are looked up in code.
const ASSETS: &[&[u8; 4]] = &[b"SPRT", b"OBJT", b"SOND", b"BGND", b"ROOM", b"SCPT", b"FONT", b"PATH", b"TMLN"];

pub struct Gms {
    d: Vec<u8>,
    pub bytecode: u32,
    chunks: Vec<Chunk>,
    /// new data, at `d.len() + offset` in the written file
    pool: Vec<u8>,
    /// STRG's list (old and new strings)
    strings: Vec<u32>,
    old_strings: usize,
    by_text: Option<HashMap<Vec<u8>, u32>>,
    vari: Vec<Entry>,
    old_vari: usize,
    func: Vec<Entry>,
    old_func: usize,
    /// FUNC's code locals (bytecode 15 on): the old ones' bytes, their count, the end of the
    /// chunk after them; then the new ones
    locals: (std::ops::Range<usize>, u32, std::ops::Range<usize>),
    new_locals: Vec<u8>,
    new_local_count: u32,
    /// pointers added to chunks' lists
    added: HashMap<[u8; 4], Vec<u32>>,
    new_code_names: Vec<String>,
    new_sprites: Vec<String>,
    pages: Vec<Page>,
    /// the longest texture page's PNG: the runner hands each page to D3DX with TXTR's size
    /// as its length, so a copy of TXTR is at least that long
    longest_png: usize,
    /// every reference in code: its instruction's address -> what it refers to (the old
    /// ones from the chains, when first needed; new ones as they are placed)
    refs: Option<HashMap<u32, Ref>>,
    new_refs: HashMap<u32, Ref>,
}

impl Gms {
    /// `d` is a whole data.win.
    pub fn parse(d: Vec<u8>) -> Result<Gms> {
        if d.get(0..4) != Some(b"FORM") {
            return Err("not a GameMaker: Studio data file (no FORM)".into());
        }
        let end = (8 + get(&d, 4)? as usize).min(d.len());
        let mut chunks = Vec::new();
        let mut o = 8;
        while o + 8 <= end {
            let size = get(&d, o + 4)? as usize;
            chunks.push(Chunk { name: d[o..o + 4].try_into().unwrap(), at: o, size });
            o += 8 + size;
        }
        let mut g = Gms {
            d,
            bytecode: 0,
            chunks,
            pool: Vec::new(),
            strings: Vec::new(),
            old_strings: 0,
            by_text: None,
            vari: Vec::new(),
            old_vari: 0,
            func: Vec::new(),
            old_func: 0,
            locals: (0..0, 0, 0..0),
            new_locals: Vec::new(),
            new_local_count: 0,
            added: HashMap::new(),
            new_code_names: Vec::new(),
            new_sprites: Vec::new(),
            pages: Vec::new(),
            longest_png: 0,
            refs: None,
            new_refs: HashMap::new(),
        };
        let gen8 = g.chunk(b"GEN8")?.data();
        g.bytecode = *g.d.get(gen8 + 1).ok_or("GEN8 too short")? as u32;
        if get(&g.d, gen8 + 44)? >= 2 {
            return Err("GameMaker Studio 2 games are not supported yet".into());
        }
        if !(14..=16).contains(&g.bytecode) {
            return Err(format!("bytecode version {} is not supported", g.bytecode));
        }
        g.strings = g.list(b"STRG")?.1;
        g.old_strings = g.strings.len();
        let new = g.bytecode >= 15;
        let vari = g.chunk(b"VARI")?;
        let (mut o, size) = if new { (vari.data() + 12, 20) } else { (vari.data(), 12) };
        while o + size <= vari.end() {
            let w = |k: usize| get(&g.d, o + 4 * k);
            let e = if new {
                Entry { name: w(0)?, inst: w(1)? as i32, id: w(2)? as i32, occurrences: w(3)?, first: w(4)?, new: Vec::new(), name_index: None }
            } else {
                Entry { name: w(0)?, inst: 0, id: 0, occurrences: w(1)?, first: w(2)?, new: Vec::new(), name_index: None }
            };
            g.vari.push(e);
            o += size;
        }
        g.old_vari = g.vari.len();
        let func = g.chunk(b"FUNC")?;
        let (mut o, end) = if new { (func.data() + 4, func.data() + 4 + 12 * get(&g.d, func.data())? as usize) } else { (func.data(), func.end()) };
        while o + 12 <= end {
            let w = |k: usize| get(&g.d, o + 4 * k);
            g.func.push(Entry { name: w(0)?, inst: 0, id: 0, occurrences: w(1)?, first: w(2)?, new: Vec::new(), name_index: None });
            o += 12;
        }
        g.old_func = g.func.len();
        if new {
            let count = get(&g.d, o)?;
            let start = o + 4;
            let mut p = start;
            for _ in 0..count {
                p += 8 + 8 * get(&g.d, p)? as usize;
            }
            if p > func.end() {
                return Err("FUNC's code locals run past the chunk".into());
            }
            g.locals = (start..p, count, p..func.end());
        }
        Ok(g)
    }

    /// The old file's bytes, for changes in place.
    pub fn form_mut(&mut self) -> &mut [u8] {
        &mut self.d
    }

    fn index(&self, name: &[u8; 4]) -> Result<usize> {
        self.chunks.iter().position(|c| &c.name == name).ok_or_else(|| format!("no {} chunk", String::from_utf8_lossy(name)))
    }

    fn chunk(&self, name: &[u8; 4]) -> Result<Chunk> {
        self.chunks.iter().find(|c| &c.name == name).copied().ok_or_else(|| format!("no {} chunk", String::from_utf8_lossy(name)))
    }

    /// A chunk's list: where it is, and its pointers.
    fn list(&self, name: &[u8; 4]) -> Result<(usize, Vec<u32>)> {
        let at = self.chunk(name)?.data();
        let n = get(&self.d, at)? as usize;
        Ok((at, (0..n).map(|i| get(&self.d, at + 4 + 4 * i)).collect::<Result<_>>()?))
    }

    fn rd(&self, a: u32) -> Result<u32> {
        let a = a as usize;
        match a.checked_sub(self.d.len()) {
            None => get(&self.d, a),
            Some(p) => get(&self.pool, p),
        }
    }

    fn wr(&mut self, a: u32, v: u32) {
        let a = a as usize;
        let at = match a.checked_sub(self.d.len()) {
            None => &mut self.d[a..a + 4],
            Some(p) => &mut self.pool[p..p + 4],
        };
        at.copy_from_slice(&v.to_le_bytes());
    }

    fn name_at(&self, p: u32) -> &[u8] {
        match (p as usize).checked_sub(self.d.len()) {
            None => c_str(&self.d, p as usize),
            Some(o) => c_str(&self.pool, o),
        }
    }

    /// Puts `bytes` in the pool at an address that is a multiple of `align`.
    fn alloc(&mut self, bytes: &[u8], align: usize) -> u32 {
        while (self.d.len() + self.pool.len()) % align != 0 {
            self.pool.push(0);
        }
        let at = self.d.len() + self.pool.len();
        self.pool.extend_from_slice(bytes);
        at as u32
    }

    // ---- strings, variables, functions -------------------------------------------------------

    /// The STRG index of `s` (added if there is none).
    pub fn string(&mut self, s: &str) -> Result<u32> {
        if self.by_text.is_none() {
            let mut map = HashMap::new();
            for (i, &p) in self.strings.iter().enumerate() {
                let len = get(&self.d, p as usize)? as usize;
                let text = self.d.get(p as usize + 4..p as usize + 4 + len).ok_or("a string past the end of the file")?;
                map.entry(text.to_vec()).or_insert(i as u32);
            }
            self.by_text = Some(map);
        }
        if let Some(&i) = self.by_text.as_ref().unwrap().get(s.as_bytes()) {
            return Ok(i);
        }
        let mut b = (s.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(s.as_bytes());
        b.push(0);
        let at = self.alloc(&b, 4);
        let i = self.strings.len() as u32;
        self.strings.push(at);
        self.by_text.as_mut().unwrap().insert(s.as_bytes().to_vec(), i);
        Ok(i)
    }

    /// A pointer to the characters of `s`, as entries refer to their names.
    pub fn string_ptr(&mut self, s: &str) -> Result<u32> {
        let i = self.string(s)?;
        Ok(self.strings[i as usize] + 4)
    }

    /// The VARI entry of built-in instance variable `name` (added if the game never uses it).
    fn builtin_var(&mut self, name: &str) -> Result<usize> {
        let new = self.bytecode >= 15;
        if let Some(i) = self.vari.iter().position(|e| (!new || (e.inst == -1 && e.id == -6)) && self.name_at(e.name) == name.as_bytes()) {
            return Ok(i);
        }
        let index = self.string(name)?;
        let name = self.strings[index as usize] + 4;
        self.vari.push(Entry { name, inst: -1, id: -6, occurrences: 0, first: u32::MAX, new: Vec::new(), name_index: Some(index) });
        Ok(self.vari.len() - 1)
    }

    /// The FUNC entry of `name` (added if the game never calls it).
    fn function(&mut self, name: &str) -> Result<usize> {
        if let Some(i) = self.func.iter().position(|e| self.name_at(e.name) == name.as_bytes()) {
            return Ok(i);
        }
        let index = self.string(name)?;
        let name = self.strings[index as usize] + 4;
        self.func.push(Entry { name, inst: 0, id: 0, occurrences: 0, first: u32::MAX, new: Vec::new(), name_index: Some(index) });
        Ok(self.func.len() - 1)
    }

    // ---- code -----------------------------------------------------------------------------

    fn code_count(&self) -> Result<usize> {
        Ok(self.list(b"CODE")?.1.len() + self.added.get(b"CODE").map_or(0, |v| v.len()))
    }

    fn has_code(&self, name: &str) -> Result<bool> {
        if self.new_code_names.iter().any(|n| n == name) {
            return Ok(true);
        }
        for p in self.list(b"CODE")?.1 {
            if self.name_at(get(&self.d, p as usize)?) == name.as_bytes() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Adds a code entry `name` holding `code`; returns its index.
    pub fn add_code(&mut self, name: &str, code: &[Ins]) -> Result<u32> {
        let (bytes, refs) = asm::assemble(code);
        let name_ptr = self.string_ptr(name)?;
        let len = bytes.len() as u32;
        let (entry, at) = if self.bytecode >= 15 {
            // the bytecode, then the entry, which points back to it
            let at = self.alloc(&bytes, 4);
            let entry = self.alloc(&[0; 20], 4);
            self.wr(entry, name_ptr);
            self.wr(entry + 4, len);
            // one local (arguments), no arguments
            self.wr(entry + 8, 1);
            self.wr(entry + 12, at.wrapping_sub(entry + 12));
            (entry, at)
        } else {
            let mut e = words(&[name_ptr, len]);
            e.extend_from_slice(&bytes);
            let entry = self.alloc(&e, 4);
            (entry, entry + 8)
        };
        self.place_refs(at, refs);
        if self.bytecode >= 15 {
            let arguments = self.string_ptr("arguments")?;
            self.new_locals.extend(words(&[1, name_ptr, 0, arguments]));
            self.new_local_count += 1;
        }
        let index = self.code_count()? as u32;
        self.added.entry(*b"CODE").or_default().push(entry);
        self.new_code_names.push(name.to_owned());
        Ok(index)
    }

    /// New code's references at `at`: links to add to their chains.
    fn place_refs(&mut self, at: u32, refs: Vec<(usize, Ref)>) {
        for (off, r) in refs {
            let ins = at + off as u32 - 4;
            match r {
                Ref::Var(i, _) => self.vari[i].new.push(ins),
                Ref::Func(i) => self.func[i].new.push(ins),
            }
            self.new_refs.insert(ins, r);
        }
    }

    /// What the reference of the instruction at `at` refers to.
    fn reference(&mut self, at: u32) -> Result<Ref> {
        if self.refs.is_none() {
            let mut map = HashMap::new();
            for (table, entries) in [(0, &self.vari), (1, &self.func)] {
                for (i, e) in entries.iter().enumerate() {
                    let mut a = e.first;
                    for _ in 0..e.occurrences {
                        let operand = get(&self.d, a as usize + 4)?;
                        map.insert(a, if table == 0 { Ref::Var(i, (operand >> 24) as u8 & 0xF8) } else { Ref::Func(i) });
                        a += operand & 0x07FF_FFFF;
                    }
                }
            }
            self.refs = Some(map);
        }
        self.refs.as_ref().unwrap().get(&at).or_else(|| self.new_refs.get(&at)).copied().ok_or_else(|| format!("code at {at:#x} refers to something no chain reaches"))
    }

    /// Appends `code` to code entry `index`, as compiling its source with `code` at the end
    /// would: its bytecode is copied, with `code` after it, and the entry points to the copy.
    fn append_code(&mut self, index: u32, code: &[Ins]) -> Result<()> {
        let bc = self.bytecode;
        let slot = self.chunk(b"CODE")?.data() as u32 + 4 + 4 * index;
        let e = self.rd(slot)?;
        let len = self.rd(e + 4)?;
        let start = if bc >= 15 { (e + 12).wrapping_add(self.rd(e + 12)?) } else { e + 8 };
        let mut ins = Vec::new();
        let mut p = start;
        while p < start + len {
            let w = self.rd(p)?;
            let (size, r) = asm::decode(bc, w);
            if r {
                ins.push(Ins::Ref(w, self.reference(p)?));
            } else {
                ins.push(Ins::Raw((0..size as u32 / 4).map(|k| self.rd(p + 4 * k)).collect::<Result<_>>()?));
            }
            p += size as u32;
        }
        if p != start + len {
            return Err("code does not end on an instruction".into());
        }
        ins.extend_from_slice(code);
        let (bytes, refs) = asm::assemble(&ins);
        let len = bytes.len() as u32;
        let at = if bc >= 15 {
            let at = self.alloc(&bytes, 4);
            self.wr(e + 4, len);
            self.wr(e + 12, at.wrapping_sub(e + 12));
            at
        } else {
            let name = self.rd(e)?;
            let mut entry = words(&[name, len]);
            entry.extend_from_slice(&bytes);
            let new = self.alloc(&entry, 4);
            self.wr(slot, new);
            new + 8
        };
        self.place_refs(at, refs);
        Ok(())
    }

    /// The code index of script `name`, if the game has it.
    pub fn script_code(&self, name: &str) -> Result<Option<u32>> {
        for p in self.list(b"SCPT")?.1 {
            if p != 0 && self.name_at(get(&self.d, p as usize)?) == name.as_bytes() {
                return Ok(Some(get(&self.d, p as usize + 4)?));
            }
        }
        Ok(None)
    }

    /// Makes code `index` end at once (`exit;` first): its first instruction becomes a
    /// branch to its end, the rest left as it is.
    pub fn end_at_once(&mut self, index: u32) -> Result<()> {
        let e = *self.list(b"CODE")?.1.get(index as usize).ok_or("no such code entry")? as usize;
        let len = get(&self.d, e + 4)?;
        let start = if self.bytecode >= 15 { (e as i64 + 12 + get(&self.d, e + 12)? as i32 as i64) as u32 } else { e as u32 + 8 };
        if len >= 4 {
            self.wr(start, asm::branch(self.bytecode, len / 4));
        }
        Ok(())
    }

    // ---- assets ---------------------------------------------------------------------------

    fn names(&self, chunk: &[u8; 4]) -> Result<Vec<Vec<u8>>> {
        let Ok((_, ptrs)) = self.list(chunk) else { return Ok(Vec::new()) };
        ptrs.iter().map(|&p| Ok(if p == 0 { Vec::new() } else { self.name_at(self.rd(p)?).to_vec() })).collect()
    }

    /// The index of asset `name` (a sprite, object, sound, ...).
    fn asset(&self, name: &str) -> Result<Option<i32>> {
        for chunk in ASSETS {
            let mut names = self.names(chunk)?;
            if *chunk == b"SPRT" {
                names.extend(self.new_sprites.iter().map(|s| s.as_bytes().to_vec()));
            }
            if let Some(i) = names.iter().position(|n| n == name.as_bytes()) {
                return Ok(Some(i as i32));
            }
        }
        Ok(None)
    }

    /// The last object called one of `names`: its entry's address.
    pub fn object(&self, names: &[&str]) -> Result<Option<usize>> {
        let (_, ptrs) = self.list(b"OBJT")?;
        let all = self.names(b"OBJT")?;
        Ok(all.iter().rposition(|n| names.iter().any(|m| n == m.as_bytes())).map(|i| ptrs[i] as usize))
    }

    /// Every object called one of `names`: (entry, name).
    pub fn objects(&self, names: &[&str]) -> Result<Vec<(usize, String)>> {
        let (_, ptrs) = self.list(b"OBJT")?;
        let all = self.names(b"OBJT")?;
        Ok(all.iter().enumerate().filter(|(_, n)| names.iter().any(|m| *n == m.as_bytes())).map(|(i, n)| (ptrs[i] as usize, String::from_utf8_lossy(n).into_owned())).collect())
    }

    pub fn depth(&self, object: usize) -> Result<i32> {
        Ok(self.rd(object as u32 + 16)? as i32)
    }

    pub fn set_depth(&mut self, object: usize, depth: i32) {
        self.wr(object as u32 + 16, depth as u32);
    }

    /// Adds `src` (compiled) to the end of an object's event `kind`/`sub` (the event is
    /// added if the object has none; the runner runs one code action per event). `tag`
    /// names new code whose usual name is taken.
    pub fn add_event_code(&mut self, object: usize, kind: usize, sub: u32, src: &str, tag: &str) -> Result<()> {
        let code = self.compile(src)?;
        let obj_name = String::from_utf8_lossy(self.name_at(self.rd(object as u32)?)).into_owned();
        // events: after the physics fields and the shape's vertices
        let vertices = self.rd(object as u32 + 64)? as usize;
        let outer = object + 80 + 8 * vertices;
        let kinds = self.rd(outer as u32)? as usize;
        if !(11..=13).contains(&kinds) || kind >= kinds {
            return Err(format!("object {obj_name}: unexpected event table ({kinds} kinds)"));
        }
        let list = self.rd((outer + 4 + 4 * kind) as u32)? as usize;
        let events = (0..self.rd(list as u32)? as usize).map(|i| self.rd((list + 4 + 4 * i) as u32)).collect::<Result<Vec<_>>>()?;
        let mut existing = None;
        for (i, &e) in events.iter().enumerate() {
            if self.rd(e)? == sub {
                existing = Some(i);
            }
        }
        // an event with code: the code gets `src` at its end
        if let Some(i) = existing {
            let e = events[i];
            let n = self.rd(e + 4)?;
            for k in (0..n).rev() {
                let action = self.rd(e + 8 + 4 * k)?;
                if self.rd(action + 4)? == 603 {
                    let index = self.rd(action + 32)?;
                    return self.append_code(index, &code);
                }
            }
        }
        let mut name = format!("gml_Object_{obj_name}_{}_{sub}", EVENTS[kind]);
        if self.has_code(&name)? {
            name = format!("{name}_{tag}");
        }
        let index = self.add_code(&name, &code)?;
        let blank = self.string_ptr("")?;
        // an "execute code" action
        let action = self.alloc(&words(&[1, 603, 7, 0, 0, 1, 2, blank, index, 1, u32::MAX, 0, 0, 0]), 4);
        match existing {
            Some(i) => {
                // (an event without code: one more action)
                let e = events[i] as usize;
                let n = self.rd(e as u32 + 4)? as usize;
                let mut w = vec![sub, n as u32 + 1];
                for k in 0..n {
                    w.push(self.rd((e + 8 + 4 * k) as u32)?);
                }
                w.push(action);
                let new = self.alloc(&words(&w), 4);
                self.wr((list + 4 + 4 * i) as u32, new);
            }
            None => {
                let event = self.alloc(&words(&[sub, 1, action]), 4);
                // in subtype order
                let mut ptrs = events.clone();
                let mut at = ptrs.len();
                for (i, &e) in events.iter().enumerate() {
                    if self.rd(e)? > sub {
                        at = i;
                        break;
                    }
                }
                ptrs.insert(at, event);
                let mut w = vec![ptrs.len() as u32];
                w.extend(ptrs);
                let new = self.alloc(&words(&w), 4);
                self.wr((outer + 4 + 4 * kind) as u32, new);
            }
        }
        Ok(())
    }

    /// Each sprite called `name` gets `frames` (and `origin`, if given); returns how many.
    pub fn replace_sprite(&mut self, name: &str, frames: &[Image], origin: Option<(i32, i32)>) -> Result<usize> {
        let (at, ptrs) = self.list(b"SPRT")?;
        let names = self.names(b"SPRT")?;
        let mut n = 0;
        for (i, s) in names.iter().enumerate() {
            if s != name.as_bytes() {
                continue;
            }
            let e = ptrs[i] as usize;
            let mut head = [0u32; 14];
            for (k, w) in head.iter_mut().enumerate() {
                *w = self.rd((e + 4 * k) as u32)?;
            }
            let new = self.sprite_entry(head, frames, origin)?;
            self.wr((at + 4 + 4 * i) as u32, new);
            n += 1;
        }
        Ok(n)
    }

    /// Adds sprite `name`; returns its index.
    pub fn add_sprite(&mut self, name: &str, frames: &[Image], origin: (i32, i32)) -> Result<u32> {
        let head = [self.string_ptr(name)?, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let e = self.sprite_entry(head, frames, Some(origin))?;
        let index = self.names(b"SPRT")?.len() + self.new_sprites.len();
        self.added.entry(*b"SPRT").or_default().push(e);
        self.new_sprites.push(name.to_owned());
        Ok(index as u32)
    }

    /// A sprite entry: `head` (name, size, margins, flags, origin) with `frames`, each on a
    /// new texture page item, and a precise collision mask per frame (from the alpha) inside
    /// the frames' bounding box.
    fn sprite_entry(&mut self, mut head: [u32; 14], frames: &[Image], origin: Option<(i32, i32)>) -> Result<u32> {
        let (w, h) = frames.first().map(|f| (f.w, f.h)).ok_or("a sprite without images")?;
        let mut bbox: Option<(u32, u32, u32, u32)> = None;
        let mut masks = Vec::new();
        for f in frames {
            let row = (w as usize).div_ceil(8);
            let mut m = vec![0u8; row * h as usize];
            for y in 0..h {
                for x in 0..w {
                    if f.rgba[((y * w + x) * 4 + 3) as usize] != 0 {
                        m[y as usize * row + x as usize / 8] |= 0x80 >> (x % 8);
                        let b = bbox.get_or_insert((x, x, y, y));
                        *b = (b.0.min(x), b.1.max(x), b.2.max(y), b.3.min(y));
                    }
                }
            }
            masks.extend(m);
        }
        while masks.len() % 4 != 0 {
            masks.push(0);
        }
        let (l, r, b, t) = bbox.unwrap_or((0, w - 1, h - 1, 0));
        head[1..7].copy_from_slice(&[w, h, l, r, b, t]);
        // precise collisions
        head[11] = 1;
        if let Some((x, y)) = origin {
            head[12] = x as u32;
            head[13] = y as u32;
        }
        let mut items = Vec::new();
        for f in frames {
            items.push(self.texture(f)?);
        }
        let mut bytes = words(&head);
        bytes.extend(words(&[items.len() as u32]));
        bytes.extend(words(&items));
        bytes.extend(words(&[frames.len() as u32]));
        bytes.extend(masks);
        Ok(self.alloc(&bytes, 4))
    }

    /// A texture page item for `img`, on a page added here.
    fn texture(&mut self, img: &Image) -> Result<u32> {
        if img.w > PAGE || img.h > PAGE || img.w == 0 || img.h == 0 {
            return Err(format!("an image of {}x{} does not fit a texture page", img.w, img.h));
        }
        let mut page = self.pages.pop().unwrap_or_default();
        if page.x + img.w > PAGE {
            page.row_y += page.row_h + GAP;
            page.x = 0;
            page.row_h = 0;
        }
        if page.row_y + img.h > PAGE {
            self.pages.push(page);
            page = Page::default();
        }
        let (x, y) = (page.x, page.row_y);
        page.x += img.w + GAP;
        page.row_h = page.row_h.max(img.h);
        page.images.push((x, y, img.clone()));
        self.pages.push(page);
        let index = self.list(b"TXTR")?.1.len() + self.pages.len() - 1;
        let (w, h) = (img.w as u16, img.h as u16);
        let item: Vec<u8> = [x as u16, y as u16, w, h, 0, 0, w, h, w, h, index as u16].iter().flat_map(|v| v.to_le_bytes()).collect();
        let at = self.alloc(&item, 4);
        self.added.entry(*b"TPAG").or_default().push(at);
        Ok(at)
    }

    /// Sets constant `name` (OPTN's `@@SleepMargin` and the like) to `value`; false if the
    /// game has no such constant.
    pub fn set_constant(&mut self, name: &str, value: &str) -> Result<bool> {
        let optn = self.chunk(b"OPTN")?;
        // the list of (name, value) string pointers, after the options (whose number
        // depends on the version)
        let valid = |p: u32| p >= 4 && (p as usize) < self.d.len() && get(&self.d, p as usize - 4).is_ok_and(|n| n as usize == c_str(&self.d, p as usize).len());
        let mut found = None;
        for at in (optn.data()..optn.end().saturating_sub(4)).step_by(4) {
            let count = get(&self.d, at)? as usize;
            if (1..=1024).contains(&count) && at + 4 + 8 * count <= optn.end() && (0..2 * count).all(|k| get(&self.d, at + 4 + 4 * k).is_ok_and(valid)) {
                found = Some((at, count));
                break;
            }
        }
        let Some((at, count)) = found else { return Ok(false) };
        for k in 0..count {
            let slot = at + 4 + 8 * k;
            if c_str(&self.d, get(&self.d, slot)? as usize) == name.as_bytes() {
                let v = self.string_ptr(value)?;
                self.wr(slot as u32 + 4, v);
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ---- the mods' code -------------------------------------------------------------------

    /// Compiles `src`: statements `variable = expression;` and `function(arguments);`, where
    /// an expression is a number, a built-in instance variable, an asset's name or a call.
    pub fn compile(&mut self, src: &str) -> Result<Vec<Ins>> {
        let toks = tokens(src)?;
        let mut p = 0;
        let mut out = Vec::new();
        while p < toks.len() {
            match (&toks[p], toks.get(p + 1)) {
                (Tok::Punct(';'), _) => p += 1,
                (Tok::Ident(n), Some(Tok::Punct('='))) => {
                    let n = n.clone();
                    p += 2;
                    let t = self.expr(&toks, &mut p, &mut out)?;
                    if !gms_builtins::INSTANCE.contains(&n.as_str()) {
                        return Err(format!("mod code: {n} is not a built-in instance variable"));
                    }
                    let v = self.builtin_var(&n)?;
                    out.push(Ins::Ref(asm::word(self.bytecode, Op::Pop, ty::VAR, t, asm::SELF), Ref::Var(v, 0xA0)));
                }
                (Tok::Ident(_), Some(Tok::Punct('('))) => {
                    self.expr(&toks, &mut p, &mut out)?;
                    out.push(Ins::Raw(vec![asm::word(self.bytecode, Op::PopZ, ty::VAR, 0, 0)]));
                }
                (t, _) => return Err(format!("mod code: unexpected {t:?}")),
            }
        }
        Ok(out)
    }

    /// One expression's code into `out`; returns its type.
    fn expr(&mut self, toks: &[Tok], p: &mut usize, out: &mut Vec<Ins>) -> Result<u32> {
        let bc = self.bytecode;
        let t = toks.get(*p).ok_or("mod code: an expression is missing")?.clone();
        *p += 1;
        let number = |s: &str, out: &mut Vec<Ins>| -> Result<u32> {
            if let Ok(i) = s.parse::<i32>() {
                if i16::try_from(i).is_ok() {
                    out.push(Ins::Raw(vec![asm::word(bc, Op::PushI, ty::INT16, 0, i as u32)]));
                } else {
                    out.push(Ins::Raw(vec![asm::word(bc, Op::Push, ty::INT32, 0, 0), i as u32]));
                }
                return Ok(ty::INT32);
            }
            let f: f64 = s.parse().map_err(|_| format!("mod code: bad number {s}"))?;
            let b = f.to_bits();
            out.push(Ins::Raw(vec![asm::word(bc, Op::Push, ty::DOUBLE, 0, 0), b as u32, (b >> 32) as u32]));
            Ok(ty::DOUBLE)
        };
        match t {
            Tok::Num(s) => number(&s, out),
            Tok::Punct('-') => match toks.get(*p) {
                Some(Tok::Num(s)) => {
                    *p += 1;
                    number(&format!("-{s}"), out)
                }
                _ => Err("mod code: '-' without a number".into()),
            },
            Tok::Ident(n) if toks.get(*p) == Some(&Tok::Punct('(')) => {
                *p += 1;
                let mut args = Vec::new();
                while toks.get(*p) != Some(&Tok::Punct(')')) {
                    let mut code = Vec::new();
                    let t = self.expr(toks, p, &mut code)?;
                    args.push((code, t));
                    match toks.get(*p) {
                        Some(Tok::Punct(',')) => *p += 1,
                        Some(Tok::Punct(')')) => {}
                        _ => return Err(format!("mod code: bad arguments to {n}")),
                    }
                }
                *p += 1;
                let argc = args.len() as u32;
                // arguments go on the stack last first, as variables
                for (code, t) in args.into_iter().rev() {
                    out.extend(code);
                    if t != ty::VAR {
                        out.push(Ins::Raw(vec![asm::word(bc, Op::Conv, t, ty::VAR, 0)]));
                    }
                }
                let f = self.function(&n)?;
                out.push(Ins::Ref(asm::word(bc, Op::Call, ty::INT32, 0, argc), Ref::Func(f)));
                Ok(ty::VAR)
            }
            Tok::Ident(n) => {
                if gms_builtins::INSTANCE.contains(&n.as_str()) {
                    let v = self.builtin_var(&n)?;
                    out.push(Ins::Ref(asm::word(bc, Op::Push, ty::VAR, 0, asm::SELF), Ref::Var(v, 0xA0)));
                    return Ok(ty::VAR);
                }
                if gms_builtins::GLOBAL.contains(&n.as_str()) {
                    return Err(format!("mod code: global built-in variables ({n}) are not supported"));
                }
                match self.asset(&n)? {
                    Some(i) => number(&i.to_string(), out),
                    None => Err(format!("mod code: {n} is neither a built-in instance variable nor an asset")),
                }
            }
            t => Err(format!("mod code: unexpected {t:?}")),
        }
    }

    // ---- writing --------------------------------------------------------------------------

    /// Links the new references onto their chains.
    fn link(&mut self) -> Result<()> {
        for table in 0..2 {
            let n = if table == 0 { self.vari.len() } else { self.func.len() };
            for i in 0..n {
                let e = if table == 0 { self.vari[i].clone() } else { self.func[i].clone() };
                if e.new.is_empty() {
                    continue;
                }
                let mut prev = None;
                let mut name_index = e.name_index;
                let mut first = e.first;
                if e.occurrences > 0 {
                    let mut a = e.first;
                    for _ in 1..e.occurrences {
                        a += self.rd(a + 4)? & 0x07FF_FFFF;
                    }
                    prev = Some(a);
                    name_index = Some(self.rd(a + 4)? & 0x07FF_FFFF);
                }
                for &a in &e.new {
                    match prev {
                        Some(p) => {
                            let delta = a - p;
                            if delta >= 1 << 27 {
                                return Err("the data file is too large for new code (a reference over 128 MB)".into());
                            }
                            let w = self.rd(p + 4)?;
                            self.wr(p + 4, (w & 0xF800_0000) | delta);
                        }
                        None => first = a,
                    }
                    prev = Some(a);
                }
                let last = prev.unwrap();
                let w = self.rd(last + 4)?;
                self.wr(last + 4, (w & 0xF800_0000) | name_index.ok_or("a chain without its name")?);
                let e = if table == 0 { &mut self.vari[i] } else { &mut self.func[i] };
                e.occurrences += e.new.len() as u32;
                e.first = first;
                e.new.clear();
            }
        }
        Ok(())
    }

    /// The pages' PNGs and their TXTR entries.
    fn flush_pages(&mut self) -> Result<()> {
        let (_, txtr) = self.list(b"TXTR")?;
        if !self.pages.is_empty() {
            for &e in &txtr {
                let png = get(&self.d, e as usize + 4)? as usize;
                self.longest_png = self.longest_png.max(png_len(&self.d, png)?);
            }
        }
        // the "scaled" word of the game's own pages
        let scaled = match txtr.first() {
            Some(&e) => get(&self.d, e as usize)?,
            None => 0,
        };
        for page in std::mem::take(&mut self.pages) {
            let w = page.images.iter().map(|(x, _, i)| x + i.w).max().unwrap_or(1).next_power_of_two();
            let h = page.images.iter().map(|(_, y, i)| y + i.h).max().unwrap_or(1).next_power_of_two();
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for (x, y, img) in &page.images {
                for row in 0..img.h {
                    let src = (row * img.w * 4) as usize;
                    let dst = (((y + row) * w + x) * 4) as usize;
                    rgba[dst..dst + (img.w * 4) as usize].copy_from_slice(&img.rgba[src..src + (img.w * 4) as usize]);
                }
            }
            let png = crate::image::encode_png(&Image { w, h, rgba })?;
            self.longest_png = self.longest_png.max(png.len());
            let at = self.alloc(&png, 0x80);
            let entry = self.alloc(&words(&[scaled, at]), 4);
            self.added.entry(*b"TXTR").or_default().push(entry);
        }
        Ok(())
    }

    /// A moved chunk's new contents.
    fn contents(&self, c: &Chunk) -> Result<Vec<u8>> {
        let new = self.bytecode >= 15;
        Ok(match &c.name {
            b"VARI" => {
                let mut out = Vec::new();
                if new {
                    let h = c.data();
                    let max_locals = get(&self.d, h + 8)?.max(1);
                    out.extend(words(&[get(&self.d, h)?, get(&self.d, h + 4)?, max_locals]));
                }
                for e in &self.vari {
                    out.extend(words(&if new { vec![e.name, e.inst as u32, e.id as u32, e.occurrences, e.first] } else { vec![e.name, e.occurrences, e.first] }));
                }
                out
            }
            b"FUNC" => {
                let mut out = Vec::new();
                if new {
                    out.extend(words(&[self.func.len() as u32]));
                }
                for e in &self.func {
                    out.extend(words(&[e.name, e.occurrences, e.first]));
                }
                if new {
                    let (old, count, tail) = &self.locals;
                    out.extend(words(&[count + self.new_local_count]));
                    out.extend_from_slice(&self.d[old.clone()]);
                    out.extend_from_slice(&self.new_locals);
                    out.extend_from_slice(&self.d[tail.clone()]);
                }
                out
            }
            b"STRG" => {
                let mut w = vec![self.strings.len() as u32];
                w.extend(&self.strings);
                words(&w)
            }
            name if self.added.contains_key(name) => {
                let mut ptrs = self.list(name)?.1;
                ptrs.extend(&self.added[name]);
                let mut w = vec![ptrs.len() as u32];
                w.extend(ptrs);
                let mut out = words(&w);
                if name == b"TXTR" && out.len() < self.longest_png {
                    out.resize(self.longest_png, 0);
                }
                out
            }
            _ => self.d[c.data()..c.end()].to_vec(),
        })
    }

    /// The changed data file.
    pub fn write(mut self) -> Result<Vec<u8>> {
        self.flush_pages()?;
        self.link()?;
        let mut grown: Vec<usize> = Vec::new();
        for (name, grows) in [
            (b"TPAG", self.added.contains_key(b"TPAG")),
            (b"CODE", self.added.contains_key(b"CODE")),
            (b"VARI", self.vari.len() > self.old_vari),
            (b"FUNC", self.func.len() > self.old_func || self.new_local_count > 0),
            (b"STRG", self.strings.len() > self.old_strings),
            (b"TXTR", self.added.contains_key(b"TXTR")),
        ] {
            if grows {
                grown.push(self.index(name)?);
            }
        }
        // the moved runs of chunks: SPRT by itself, the rest together
        let mut runs: Vec<(usize, usize)> = Vec::new();
        if self.added.contains_key(b"SPRT") {
            let s = self.index(b"SPRT")?;
            runs.push((s, s));
        }
        if let (Some(&a), Some(&b)) = (grown.iter().min(), grown.iter().max()) {
            runs.push((a, b));
        }
        // the chunk before a run takes its old bytes: a list the runner reads by its count
        let absorbs = |c: &Chunk| c.size >= 4 && [b"AGRP", b"SOND", b"SPRT", b"BGND", b"PATH", b"SCPT", b"TMLN", b"OBJT", b"ROOM", b"TPAG", b"CODE", b"STRG"].contains(&&c.name);
        for r in runs.iter_mut() {
            while r.0 > 1 && !absorbs(&self.chunks[r.0 - 1]) {
                r.0 -= 1;
            }
            if r.0 <= 1 {
                return Err("no chunk to take a moved chunk's place".into());
            }
        }
        runs.sort();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for r in runs {
            match merged.last_mut() {
                Some(m) if r.0 <= m.1 + 1 => m.1 = m.1.max(r.1),
                _ => merged.push(r),
            }
        }
        let moved = |i: usize| merged.iter().any(|r| (r.0..=r.1).contains(&i));
        // VARI and FUNC entries that stay: their counts and first addresses in place
        let new = self.bytecode >= 15;
        for (name, table) in [(b"VARI", 0), (b"FUNC", 1)] {
            let i = self.index(name)?;
            if moved(i) {
                continue;
            }
            let c = self.chunks[i];
            let (start, size, occ) = match (table, new) {
                (0, true) => (c.data() + 12, 20, 12),
                (0, false) => (c.data(), 12, 4),
                (_, true) => (c.data() + 4, 12, 4),
                (_, false) => (c.data(), 12, 4),
            };
            let entries = if table == 0 { self.vari.clone() } else { self.func.clone() };
            for (k, e) in entries.iter().enumerate() {
                let at = (start + k * size + occ) as u32;
                self.wr(at, e.occurrences);
                self.wr(at + 4, e.first);
            }
        }
        // sizes: each run's bytes go to the chunk before it; the pool to the chunk that
        // then ends the old file
        let mut sizes: Vec<usize> = self.chunks.iter().map(|c| c.size).collect();
        for r in &merged {
            sizes[r.0 - 1] += self.chunks[r.1].end() - self.chunks[r.0].at;
        }
        // (new chunks start at a multiple of 4)
        while (!self.pool.is_empty() || !merged.is_empty()) && (self.d.len() + self.pool.len()) % 4 != 0 {
            self.pool.push(0);
        }
        let mut last = self.chunks.len() - 1;
        if let Some(r) = merged.iter().find(|r| r.1 == last) {
            last = r.0 - 1;
        }
        sizes[last] += self.pool.len();
        let mut tail = Vec::new();
        for r in &merged {
            for i in r.0..=r.1 {
                let c = self.chunks[i];
                let mut body = self.contents(&c)?;
                while body.len() % 4 != 0 {
                    body.push(0);
                }
                tail.extend_from_slice(&c.name);
                tail.extend_from_slice(&(body.len() as u32).to_le_bytes());
                tail.extend(body);
            }
        }
        let mut out = std::mem::take(&mut self.d);
        for (c, &size) in self.chunks.iter().zip(&sizes) {
            out[c.at + 4..c.at + 8].copy_from_slice(&(size as u32).to_le_bytes());
        }
        out.extend_from_slice(&self.pool);
        out.extend(tail);
        let form = (out.len() - 8) as u32;
        out[4..8].copy_from_slice(&form.to_le_bytes());
        Ok(out)
    }
}

/// The length of the PNG at `at` (its chunks, to IEND).
fn png_len(d: &[u8], at: usize) -> Result<usize> {
    if d.get(at..at + 8) != Some(b"\x89PNG\r\n\x1a\n") {
        return Err(format!("no PNG at {at:#x}"));
    }
    let mut p = at + 8;
    loop {
        let len = u32::from_be_bytes(d.get(p..p + 4).ok_or("a PNG past the end of the file")?.try_into().unwrap()) as usize;
        let kind = d.get(p + 4..p + 8).ok_or("a PNG past the end of the file")?;
        p += 12 + len;
        if kind == b"IEND" {
            return Ok(p - at);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Num(String),
    Punct(char),
}

fn tokens(src: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let c: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_alphabetic() || ch == '_' {
            let s = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(c[s..i].iter().collect()));
        } else if ch.is_ascii_digit() || ch == '.' {
            let s = i;
            while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                i += 1;
            }
            out.push(Tok::Num(c[s..i].iter().collect()));
        } else if "()=,;-".contains(ch) {
            out.push(Tok::Punct(ch));
            i += 1;
        } else {
            return Err(format!("mod code: unexpected '{ch}'"));
        }
    }
    Ok(out)
}
