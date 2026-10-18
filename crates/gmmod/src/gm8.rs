//! GameMaker 8.0/8.1 game data inside the runner exe: found, decrypted, walked to its asset
//! entries, and written back with only the entries a mod changed rebuilt (every other
//! entry keeps its compressed bytes, and names and code stay the bytes they were).
//!
//! The layout, the 8.1 encryption and the way the data is written back (an identity swap
//! table, the 8.1 XOR stream applied again, the 8.1 runner's check of the data turned off)
//! follow Fangame.ModLoader's GM8Data.cs, itself after OpenGMK.

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use std::io::{Read, Write};

pub type Result<T> = std::result::Result<T, String>;

pub struct Reader<'a> {
    pub d: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        Reader { d, pos: 0 }
    }
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.d.get(self.pos..self.pos + 4).ok_or("GM8 data ends early")?;
        self.pos += 4;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let b = self.d.get(self.pos..self.pos.checked_add(n).ok_or("bad length")?).ok_or("GM8 data ends early")?;
        self.pos += n;
        Ok(b)
    }
    /// A length-prefixed string or blob.
    pub fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
}

#[derive(Default)]
pub struct Writer {
    pub d: Vec<u8>,
}

impl Writer {
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.d.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.u32(v as u32)
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u32(v as u32)
    }
    pub fn string(&mut self, s: &[u8]) -> &mut Self {
        self.u32(s.len() as u32);
        self.d.extend_from_slice(s);
        self
    }
    pub fn raw(&mut self, s: &[u8]) -> &mut Self {
        self.d.extend_from_slice(s);
        self
    }
}

pub fn inflate(z: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(z).read_to_end(&mut out).map_err(|e| format!("GM8 data: {e}"))?;
    Ok(out)
}

pub fn deflate(d: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
    e.write_all(d).unwrap();
    e.finish().unwrap()
}

/// An asset entry: as stored (zlib bytes) or rebuilt (its content, from the exists flag on).
#[derive(Clone)]
pub enum Entry {
    Stored(Vec<u8>),
    Content(Vec<u8>),
}

impl Entry {
    pub fn content(&self) -> Result<Vec<u8>> {
        match self {
            Entry::Stored(z) => inflate(z),
            Entry::Content(c) => Ok(c.clone()),
        }
    }
}

/// A list of compressed asset entries (their position is the asset's index).
#[derive(Clone)]
pub struct List {
    pub version: u32,
    pub entries: Vec<Entry>,
}

impl List {
    fn read(r: &mut Reader) -> Result<List> {
        let version = r.u32()?;
        let n = r.u32()? as usize;
        let mut entries = Vec::with_capacity(n.min(100_000));
        for _ in 0..n {
            entries.push(Entry::Stored(r.string()?.to_vec()));
        }
        Ok(List { version, entries })
    }
    fn write(&self, w: &mut Writer) {
        w.u32(self.version).u32(self.entries.len() as u32);
        for e in &self.entries {
            match e {
                Entry::Stored(z) => w.string(z),
                Entry::Content(c) => w.string(&deflate(c)),
            };
        }
    }
    /// The name of each entry that exists (None for empty slots).
    pub fn names(&self) -> Result<Vec<Option<Vec<u8>>>> {
        self.entries
            .iter()
            .map(|e| {
                let c = e.content()?;
                let mut r = Reader::new(&c);
                if r.u32()? == 0 {
                    return Ok(None);
                }
                Ok(Some(r.string()?.to_vec()))
            })
            .collect()
    }
    /// The index of the asset called `name`.
    pub fn find(&self, name: &str) -> Result<Option<usize>> {
        Ok(self.names()?.iter().position(|n| n.as_deref() == Some(name.as_bytes())))
    }
}

/// GM8.1's XOR layer: a keystream from two seeds, over the file from `start` on.
#[derive(Clone)]
struct Xor {
    start: usize,
    seed1: u32,
    seed2: u32,
    iter: Option<Vec<u32>>,
}

impl Xor {
    fn apply(&self, d: &mut [u8]) {
        let (mut s1, mut s2) = (self.seed1, self.seed2);
        let mut k = 0;
        let mut off = self.start;
        while off + 4 <= d.len() {
            let (n1, n2) = match &self.iter {
                None => (0x9069, 0x4650),
                Some(it) => {
                    let a = it[k % it.len()];
                    let b = it[(k + 1) % it.len()];
                    k = (k + 2) % it.len();
                    (a, b)
                }
            };
            s1 = (s1 & 0xFFFF).wrapping_mul(n1).wrapping_add(s1 >> 16);
            s2 = (s2 & 0xFFFF).wrapping_mul(n2).wrapping_add(s2 >> 16);
            let mask = (s1 << 16).wrapping_add(s2 & 0xFFFF);
            let v = u32::from_le_bytes(d[off..off + 4].try_into().unwrap()) ^ mask;
            d[off..off + 4].copy_from_slice(&v.to_le_bytes());
            off += 4;
        }
    }
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

/// CRC-32 (reflected, polynomial 0x04C11DB7), started at all ones and not inverted at the end.
fn crc32_open(d: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    let mut r = 0xFFFF_FFFFu32;
    for &b in d {
        r = (r >> 8) ^ table[((r & 0xFF) ^ b as u32) as usize];
    }
    r
}

/// GM8.1 (the runner build the importer recognises): the header's position `x` and the
/// XOR layer, or None.
fn find_gm81(d: &[u8]) -> Result<Option<(usize, Xor)>> {
    if d.len() < 0x226D8A || d[0x226CF3..0x226CFB] != [0xE8, 0x80, 0xF2, 0xDD, 0xFF, 0xC7, 0x45, 0xF0] {
        return Ok(None);
    }
    let header_start = u32_at(d, 0x226CFB).unwrap() as usize;
    let magic = (d[0x226D7C..0x226D7F] == [0x81, 0x7D, 0xEC] && d[0x226D83] == 0x74).then(|| u32_at(d, 0x226D7F).unwrap());
    let sudalv = d[0x10BB83..0x10BB8B] == [0x8B, 0x02, 0xC1, 0xE0, 0x10, 0x8B, 0x11, 0x81];
    let x = match magic {
        Some(m) => {
            let mut p = header_start;
            loop {
                if p + 8 >= d.len() {
                    return Ok(None);
                }
                let (d1, d2) = (u32_at(d, p).unwrap(), u32_at(d, p + 4).unwrap());
                if (d1 & 0xFF00_FF00) | (d2 & 0x00FF_00FF) == m {
                    break p + 8;
                }
                p += 1;
            }
        }
        None => header_start + 8,
    };
    let key = format!("_MJD{}#RWK", u32_at(d, x).ok_or("GM8.1 header past the end")? as i32);
    let key16: Vec<u8> = key.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let seed2 = crc32_open(&key16);
    let seed1 = u32_at(d, x + 4).ok_or("GM8.1 header past the end")?;
    let start = x + 8 + (seed2 & 0xFF) as usize + 10;
    let iter = if sudalv {
        // from the end of the bytes before the header, 16-bit values up to two zero pairs
        let m = d.get(..x.saturating_sub(8)).unwrap_or(&[]);
        let pairs: Vec<(u8, u8)> = (0..m.len() / 2).map(|j| m.len() - 2 - 2 * j).map(|i| (m[i], m[i + 1])).collect();
        let count = (0..pairs.len().saturating_sub(1)).find(|&j| pairs[j + 1] == (0, 0) && pairs[j] == (0, 0)).ok_or("GM8.1: no key table")?;
        Some((1..count + 2).map(|i| pairs[i].0 as u32 | (pairs[i].1 as u32) << 8).collect())
    } else {
        None
    };
    Ok(Some((x, Xor { start, seed1, seed2, iter })))
}

/// GM8.0: the settings' position, after the magic number.
fn find_gm80(d: &[u8]) -> Option<usize> {
    if u32_at(d, 1_980_000) == Some(1_234_321) {
        return Some(1_980_000 + 16);
    }
    let mut p = 2_000_000;
    while p < d.len() {
        if u32_at(d, p) == Some(1_234_321) {
            return Some(p + 16);
        }
        p += 10_000;
    }
    None
}

/// A GM8 game's exe, its game data parsed to entries.
pub struct Gm8 {
    /// 800 or 810
    pub version: u32,
    /// the exe, the 8.1 XOR layer removed
    exe: Vec<u8>,
    xor: Option<Xor>,
    settings_pos: usize,
    /// the settings, decompressed, once a mod changed them
    pub settings: Option<Vec<u8>>,
    settings_stored: Vec<u8>,
    /// the DirectX 8 DLL's name and bytes, as stored
    dx8: Vec<u8>,
    head: Vec<u8>,
    extensions: Vec<u8>,
    pub triggers: List,
    constants: Vec<u8>,
    pub sounds: List,
    pub sprites: List,
    pub backgrounds: List,
    pub paths: List,
    pub scripts: List,
    pub fonts: List,
    pub timelines: List,
    pub objects: List,
    pub rooms: List,
    ids: Vec<u8>,
    pub included: List,
    tail: Vec<u8>,
}

impl Gm8 {
    pub fn parse(exe: &[u8]) -> Result<Gm8> {
        let mut exe = exe.to_vec();
        let (version, xor, settings_pos) = match find_gm81(&exe)? {
            Some((x, xor)) => {
                xor.apply(&mut exe);
                (810, Some(xor), x + 28)
            }
            None => (800, None, find_gm80(&exe).ok_or("not a GameMaker 8 game (no game data found)")?),
        };
        let mut r = Reader::new(&exe);
        r.pos = settings_pos;
        let settings_stored = r.string()?.to_vec();
        let dx8_start = r.pos;
        r.string()?;
        r.string()?;
        let dx8 = exe[dx8_start..r.pos].to_vec();
        let (g1, g2) = (r.u32()? as usize, r.u32()? as usize);
        r.skip(g1 * 4)?;
        let table: [u8; 256] = r.bytes(256)?.try_into().unwrap();
        r.skip(g2 * 4)?;
        let mut blob = r.string()?.to_vec();
        // the swap-table cipher
        let mut rev = [0u8; 256];
        for (i, &t) in table.iter().enumerate() {
            rev[t as usize] = i as u8;
        }
        for k in (1..blob.len()).rev() {
            blob[k] = rev[blob[k] as usize].wrapping_sub(blob[k - 1].wrapping_add(k as u8));
        }
        for k in (0..blob.len()).rev() {
            let j = k.saturating_sub(table[k & 0xFF] as usize);
            blob.swap(k, j);
        }

        let mut r = Reader::new(&blob);
        let junk = r.u32()? as usize;
        r.skip(junk * 4)?;
        r.u32()?; // a flag
        r.u32()?; // game id
        r.skip(16)?; // guid
        let head = blob[..r.pos].to_vec();
        let start = r.pos;
        // extensions (stored uncompressed): walked to find their end
        r.u32()?;
        for _ in 0..r.u32()? {
            r.u32()?;
            r.string()?;
            r.string()?;
            for _ in 0..r.u32()? {
                r.u32()?;
                r.string()?;
                r.u32()?;
                r.string()?;
                r.string()?;
                for _ in 0..r.u32()? {
                    r.u32()?;
                    r.string()?;
                    r.string()?;
                    r.skip(4 * (3 + 17 + 1))?;
                }
                for _ in 0..r.u32()? {
                    r.u32()?;
                    r.string()?;
                    r.string()?;
                }
            }
            r.string()?;
        }
        let extensions = blob[start..r.pos].to_vec();
        let triggers = List::read(&mut r)?;
        let start = r.pos;
        r.u32()?;
        for _ in 0..r.u32()? {
            r.string()?;
            r.string()?;
        }
        let constants = blob[start..r.pos].to_vec();
        let sounds = List::read(&mut r)?;
        let sprites = List::read(&mut r)?;
        let backgrounds = List::read(&mut r)?;
        let paths = List::read(&mut r)?;
        let scripts = List::read(&mut r)?;
        let fonts = List::read(&mut r)?;
        let timelines = List::read(&mut r)?;
        let objects = List::read(&mut r)?;
        let rooms = List::read(&mut r)?;
        let ids = r.bytes(8)?.to_vec();
        let included = List::read(&mut r)?;
        let tail = blob[r.pos..].to_vec();
        Ok(Gm8 {
            version,
            exe,
            xor,
            settings_pos,
            settings: None,
            settings_stored,
            dx8,
            head,
            extensions,
            triggers,
            constants,
            sounds,
            sprites,
            backgrounds,
            paths,
            scripts,
            fonts,
            timelines,
            objects,
            rooms,
            ids,
            included,
            tail,
        })
    }

    /// The settings, decompressed (to change them).
    pub fn settings_mut(&mut self) -> Result<&mut Vec<u8>> {
        if self.settings.is_none() {
            self.settings = Some(inflate(&self.settings_stored)?);
        }
        Ok(self.settings.as_mut().unwrap())
    }

    /// The exe with the game data written back.
    pub fn write(&self) -> Vec<u8> {
        let mut b = Writer::default();
        b.raw(&self.head).raw(&self.extensions);
        self.triggers.write(&mut b);
        b.raw(&self.constants);
        for l in [&self.sounds, &self.sprites, &self.backgrounds, &self.paths, &self.scripts, &self.fonts, &self.timelines, &self.objects, &self.rooms] {
            l.write(&mut b);
        }
        b.raw(&self.ids);
        self.included.write(&mut b);
        b.raw(&self.tail);
        let mut blob = b.d;
        // the swap-table cipher with an identity table
        for i in 0..blob.len() {
            blob.swap(i, i & !0xFF);
        }
        for i in 1..blob.len() {
            blob[i] = blob[i].wrapping_add(blob[i - 1]).wrapping_add(i as u8);
        }

        let mut w = Writer::default();
        w.raw(&self.exe[..self.settings_pos]);
        match &self.settings {
            Some(s) => w.string(&deflate(s)),
            None => w.string(&self.settings_stored),
        };
        w.raw(&self.dx8).u32(0).u32(0);
        for i in 0..=255u8 {
            w.d.push(i);
        }
        w.string(&blob);
        let mut out = w.d;
        if let Some(xor) = &self.xor {
            xor.apply(&mut out);
            // the 8.1 runner's check of its game data
            if out.get(0x10BD49) == Some(&0x74) {
                out[0x10BD49] = 0xEB;
            }
        }
        out
    }
}

// ---- assets ------------------------------------------------------------------------------

/// A script: its name and code.
pub struct Script {
    pub name: Vec<u8>,
    pub source: Vec<u8>,
}

impl Script {
    pub fn parse(c: &[u8]) -> Result<Option<Script>> {
        let mut r = Reader::new(c);
        if r.u32()? == 0 {
            return Ok(None);
        }
        let name = r.string()?.to_vec();
        r.u32()?;
        Ok(Some(Script { name, source: r.string()?.to_vec() }))
    }
    pub fn content(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(1).string(&self.name).u32(800).string(&self.source);
        w.d
    }
}

/// An image in RGBA rows.
#[derive(Clone)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// A sprite's fields this code changes; the frames as stored unless replaced.
pub struct Sprite {
    pub name: Vec<u8>,
    pub origin: (i32, i32),
    frames: Vec<u8>,
    frame_count: u32,
    sep_masks: bool,
    masks: Vec<u8>,
}

impl Sprite {
    pub fn parse(c: &[u8]) -> Result<Option<Sprite>> {
        let mut r = Reader::new(c);
        if r.u32()? == 0 {
            return Ok(None);
        }
        let name = r.string()?.to_vec();
        r.u32()?;
        let origin = (r.i32()?, r.i32()?);
        let frame_count = r.u32()?;
        let start = r.pos;
        for _ in 0..frame_count {
            r.u32()?;
            r.u32()?;
            r.u32()?;
            r.string()?;
        }
        let frames = c[start..r.pos].to_vec();
        // (the flag and the masks only follow frames)
        let sep_masks = frame_count > 0 && r.u32()? != 0;
        let masks = c[r.pos..].to_vec();
        Ok(Some(Sprite { name, origin, frames, frame_count, sep_masks, masks }))
    }

    /// A new sprite.
    pub fn new(name: &str) -> Sprite {
        Sprite { name: name.as_bytes().to_vec(), origin: (0, 0), frames: Vec::new(), frame_count: 0, sep_masks: true, masks: Vec::new() }
    }

    /// Replaces the frames, with collision masks from their alpha (a pixel counts when it
    /// is not fully transparent; one mask for all frames unless the sprite keeps one per
    /// frame).
    pub fn set_frames(&mut self, frames: &[Image]) {
        let mut f = Writer::default();
        for im in frames {
            let bgra: Vec<u8> = im.rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
            f.u32(800).u32(im.w).u32(im.h).string(&bgra);
        }
        self.frames = f.d;
        self.frame_count = frames.len() as u32;
        let mut m = Writer::default();
        let mask = |m: &mut Writer, w: u32, h: u32, set: &dyn Fn(usize) -> bool| {
            let (mut l, mut r, mut t, mut b) = (w, 0, h, 0);
            for y in 0..h {
                for x in 0..w {
                    if set((y * w + x) as usize) {
                        l = l.min(x);
                        r = r.max(x);
                        t = t.min(y);
                        b = b.max(y);
                    }
                }
            }
            if l > r {
                (l, r, t, b) = (0, w.saturating_sub(1), 0, h.saturating_sub(1));
            }
            m.u32(800).u32(w).u32(h).u32(l).u32(r).u32(b).u32(t);
            for i in 0..(w * h) as usize {
                m.bool(set(i));
            }
        };
        if let Some(first) = frames.first() {
            if self.sep_masks {
                for im in frames {
                    mask(&mut m, im.w, im.h, &|i| im.rgba[i * 4 + 3] != 0);
                }
            } else {
                mask(&mut m, first.w, first.h, &|i| frames.iter().any(|im| im.rgba.get(i * 4 + 3).is_some_and(|&a| a != 0)));
            }
        }
        self.masks = m.d;
    }

    pub fn content(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(1).string(&self.name).u32(800).i32(self.origin.0).i32(self.origin.1).u32(self.frame_count).raw(&self.frames);
        if self.frame_count > 0 {
            w.bool(self.sep_masks).raw(&self.masks);
        }
        w.d
    }
}

/// Event types (GameMaker's numbering, shared with Studio).
pub mod event {
    pub const CREATE: usize = 0;
    pub const STEP: usize = 3;
    pub const OTHER: usize = 7;
    pub const DRAW: usize = 8;
    /// Step subtypes
    pub const END_STEP: u32 = 2;
    /// Other subtypes
    pub const GAME_END: u32 = 3;
}

/// An object; its events' actions as stored.
pub struct Object {
    pub name: Vec<u8>,
    pub sprite: i32,
    pub solid: bool,
    pub visible: bool,
    pub depth: i32,
    pub persistent: bool,
    pub parent: i32,
    pub mask: i32,
    /// per event type: (subtype, action count, the actions' bytes)
    pub events: Vec<Vec<(u32, u32, Vec<u8>)>>,
}

fn skip_action(r: &mut Reader) -> Result<()> {
    r.skip(4 * 8)?; // version, library, id, kind, relative?, question?, applies?, execution
    r.string()?;
    r.string()?;
    r.u32()?; // parameter count
    let types = r.u32()? as usize;
    r.skip(4 * types)?;
    r.skip(4 * 2)?; // applies to, relative
    for _ in 0..r.u32()? {
        r.string()?;
    }
    r.u32()?; // not
    Ok(())
}

/// A "code" action (library 1, action 603) running `code`.
pub fn code_action(code: &[u8]) -> Vec<u8> {
    let mut w = Writer::default();
    w.u32(440).u32(1).u32(603).u32(7).bool(false).bool(false).bool(true).u32(2).string(b"").string(b"").u32(1).u32(8);
    w.u32(1);
    for _ in 0..7 {
        w.u32(0);
    }
    w.i32(-1).bool(false).u32(8).string(code);
    for _ in 0..7 {
        w.string(b"");
    }
    w.bool(false);
    w.d
}

impl Object {
    pub fn new(name: &str) -> Object {
        Object { name: name.as_bytes().to_vec(), sprite: -1, solid: false, visible: false, depth: 0, persistent: false, parent: -100, mask: -1, events: vec![Vec::new(); 12] }
    }

    pub fn parse(c: &[u8]) -> Result<Option<Object>> {
        let mut r = Reader::new(c);
        if r.u32()? == 0 {
            return Ok(None);
        }
        let name = r.string()?.to_vec();
        r.u32()?;
        let sprite = r.i32()?;
        let solid = r.u32()? != 0;
        let visible = r.u32()? != 0;
        let depth = r.i32()?;
        let persistent = r.u32()? != 0;
        let parent = r.i32()?;
        let mask = r.i32()?;
        let types = r.u32()? as usize + 1;
        let mut events = Vec::with_capacity(types);
        for _ in 0..types {
            let mut list = Vec::new();
            loop {
                let sub = r.i32()?;
                if sub == -1 {
                    break;
                }
                r.u32()?;
                let n = r.u32()?;
                let start = r.pos;
                for _ in 0..n {
                    skip_action(&mut r)?;
                }
                list.push((sub as u32, n, c[start..r.pos].to_vec()));
            }
            events.push(list);
        }
        Ok(Some(Object { name, sprite, solid, visible, depth, persistent, parent, mask, events }))
    }

    /// Appends a code action running `code` to event `kind`/`sub` (made if missing).
    pub fn add_code(&mut self, kind: usize, sub: u32, code: &[u8]) {
        while self.events.len() <= kind {
            self.events.push(Vec::new());
        }
        let list = &mut self.events[kind];
        let i = match list.iter().position(|e| e.0 == sub) {
            Some(i) => i,
            None => {
                list.push((sub, 0, Vec::new()));
                list.len() - 1
            }
        };
        list[i].1 += 1;
        list[i].2.extend_from_slice(&code_action(code));
    }

    pub fn content(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u32(1).string(&self.name).u32(430).i32(self.sprite).bool(self.solid).bool(self.visible).i32(self.depth).bool(self.persistent).i32(self.parent).i32(self.mask);
        w.u32(self.events.len() as u32 - 1);
        for list in &self.events {
            for (sub, n, actions) in list {
                w.u32(*sub).u32(400).u32(*n).raw(actions);
            }
            w.i32(-1);
        }
        w.d
    }
}
