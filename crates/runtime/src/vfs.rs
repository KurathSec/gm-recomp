//! The guest's file system. Paths are guest paths relative to the game directory
//! (`C:\game`), as components joined by `\`; lookups are case-insensitive like
//! Windows. `DirFs` serves a host directory with a copy-on-write layer (the harness);
//! `MemFs` keeps everything in memory (the browser, which persists it itself).

use std::collections::BTreeMap;
use crate::fasthash::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default)]
pub struct Meta {
    pub is_dir: bool,
    pub size: u64,
    /// Last write time as a FILETIME (0 if unknown).
    pub mtime: u64,
}

pub trait Vfs {
    fn stat(&self, path: &str) -> Option<Meta>;
    /// Entries of a directory: (name as stored, metadata).
    fn list(&self, dir: &str) -> Vec<(String, Meta)>;
    /// Reads at `pos`; returns the number of bytes read (0 at the end).
    fn read_at(&self, path: &str, pos: u64, buf: &mut [u8]) -> Option<usize>;
    /// Writes at `pos`, extending the file (with zeros) as needed.
    fn write_at(&mut self, path: &str, pos: u64, data: &[u8]) -> bool;
    /// Creates an empty file or truncates an existing one.
    fn create(&mut self, path: &str) -> bool;
    fn set_len(&mut self, path: &str, len: u64) -> bool;
    fn mkdir(&mut self, path: &str) -> bool;
    /// Removes a file or an empty directory.
    fn remove(&mut self, path: &str) -> bool;
    fn rename(&mut self, from: &str, to: &str) -> bool;

    /// Files changed since the last call, for hosts that persist them: (path as
    /// stored, Some((contents, FILETIME)) or None if it was removed). Directories are
    /// implied by their files.
    fn take_changes(&mut self) -> Vec<(String, Option<(Vec<u8>, u64)>)> {
        Vec::new()
    }

    fn exists(&self, path: &str) -> bool {
        self.stat(path).is_some()
    }
    fn is_file(&self, path: &str) -> bool {
        self.stat(path).is_some_and(|m| !m.is_dir)
    }
    fn is_dir(&self, path: &str) -> bool {
        path.is_empty() || self.stat(path).is_some_and(|m| m.is_dir)
    }
    fn read_all(&self, path: &str) -> Option<Vec<u8>> {
        let m = self.stat(path)?;
        if m.is_dir {
            return None;
        }
        let mut buf = vec![0u8; m.size as usize];
        let n = self.read_at(path, 0, &mut buf)?;
        buf.truncate(n);
        Some(buf)
    }
    fn copy(&mut self, from: &str, to: &str) -> bool {
        match self.read_all(from) {
            Some(d) => self.create(to) && self.write_at(to, 0, &d),
            None => false,
        }
    }
}

/// Directories every Windows system has (relative to `C:\\game`, drive roots below it).
/// Where a host keeps the files a game writes (its saves) between sessions.
pub trait SaveSink {
    /// Changed files: (path as stored, Some((contents, FILETIME)) or None if removed).
    fn save(&mut self, _changes: &[(String, Option<(Vec<u8>, u64)>)]) {}
}

pub struct NullSaves;
impl SaveSink for NullSaves {}

pub const STANDARD_DIRS: &[&str] = &[
    "C",
    "C\\Temp",
    "C\\Windows",
    "C\\Windows\\system32",
    "C\\Windows\\Fonts",
    "C\\ProgramData",
    "C\\Program Files",
    "C\\Program Files\\Common Files",
    "C\\Users",
    "C\\Users\\player",
    "C\\Users\\player\\Desktop",
    "C\\Users\\player\\Documents",
    "C\\Users\\player\\Music",
    "C\\Users\\player\\Pictures",
    "C\\Users\\player\\Videos",
    "C\\Users\\player\\AppData",
    "C\\Users\\player\\AppData\\Roaming",
    "C\\Users\\player\\AppData\\Local",
    "C\\Users\\player\\AppData\\Local\\Temp",
];

/// Creates the standard directories that are missing.
pub fn add_standard_dirs(fs: &mut dyn Vfs) {
    for d in STANDARD_DIRS {
        if !fs.exists(d) {
            fs.mkdir(d);
        }
    }
}

pub fn parent(path: &str) -> &str {
    path.rsplit_once('\\').map(|(d, _)| d).unwrap_or("")
}

fn unix_to_filetime(secs: u64, nanos: u32) -> u64 {
    (secs + 11_644_473_600) * 10_000_000 + (nanos / 100) as u64
}

// ---- host directory with a copy-on-write layer ------------------------------------------------

pub struct DirFs {
    pub root: PathBuf,
    /// Writes go here; None writes into `root`.
    pub overlay: Option<PathBuf>,
    /// Paths (lowercase) deleted from the read-only layer.
    deleted: HashSet<String>,
}

impl DirFs {
    pub fn new(root: PathBuf, overlay: Option<PathBuf>) -> Self {
        DirFs { root, overlay, deleted: HashSet::default() }
    }

    /// Finds `path` under `base`, matching each component case-insensitively.
    fn resolve_in(base: &Path, path: &str) -> PathBuf {
        let mut out = base.to_path_buf();
        for comp in path.split('\\').filter(|c| !c.is_empty()) {
            let exact = out.join(comp);
            if exact.exists() {
                out = exact;
                continue;
            }
            let found = std::fs::read_dir(&out).ok().and_then(|rd| {
                rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(comp)).map(|e| e.path())
            });
            out = found.unwrap_or(exact);
        }
        out
    }

    /// The host path that currently holds `path` (overlay first).
    fn read_path(&self, path: &str) -> PathBuf {
        if let Some(o) = &self.overlay {
            let p = Self::resolve_in(o, path);
            if p.exists() || self.deleted.contains(&path.to_lowercase()) {
                return p;
            }
        }
        Self::resolve_in(&self.root, path)
    }

    /// The host path to write `path` at, copying it up into the overlay first.
    fn write_path(&mut self, path: &str) -> PathBuf {
        let Some(o) = self.overlay.clone() else { return Self::resolve_in(&self.root, path) };
        let key = path.to_lowercase();
        let wp = Self::resolve_in(&o, path);
        if !wp.exists() && !self.deleted.contains(&key) {
            let base = Self::resolve_in(&self.root, path);
            if let Some(parent) = wp.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if base.is_file() {
                let _ = std::fs::copy(&base, &wp);
            } else if base.is_dir() {
                let _ = std::fs::create_dir_all(&wp);
            }
        } else if let Some(parent) = wp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.deleted.remove(&key);
        wp
    }

    fn meta(p: &Path) -> Option<Meta> {
        let m = std::fs::metadata(p).ok()?;
        let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| unix_to_filetime(d.as_secs(), d.subsec_nanos())).unwrap_or(0);
        Some(Meta { is_dir: m.is_dir(), size: m.len(), mtime })
    }
}

impl Vfs for DirFs {
    fn stat(&self, path: &str) -> Option<Meta> {
        if self.deleted.contains(&path.to_lowercase()) {
            return None;
        }
        Self::meta(&self.read_path(path))
    }

    fn list(&self, dir: &str) -> Vec<(String, Meta)> {
        let mut out: Vec<(String, Meta)> = Vec::new();
        let mut roots = vec![self.root.clone()];
        if let Some(o) = &self.overlay {
            roots.insert(0, o.clone());
        }
        for root in roots {
            let Ok(rd) = std::fs::read_dir(Self::resolve_in(&root, dir)) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let full = if dir.is_empty() { name.clone() } else { format!("{dir}\\{name}") };
                if self.deleted.contains(&full.to_lowercase()) || out.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                    continue;
                }
                if let Some(m) = Self::meta(&e.path()) {
                    out.push((name, m));
                }
            }
        }
        out
    }

    fn read_at(&self, path: &str, pos: u64, buf: &mut [u8]) -> Option<usize> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(self.read_path(path)).ok()?;
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut got = 0;
        while got < buf.len() {
            match f.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(_) => break,
            }
        }
        Some(got)
    }

    fn write_at(&mut self, path: &str, pos: u64, data: &[u8]) -> bool {
        use std::io::{Seek, SeekFrom, Write};
        let p = self.write_path(path);
        let Ok(mut f) = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&p) else { return false };
        f.seek(SeekFrom::Start(pos)).is_ok() && f.write_all(data).is_ok()
    }

    fn create(&mut self, path: &str) -> bool {
        let p = self.write_path(path);
        std::fs::File::create(p).is_ok()
    }

    fn set_len(&mut self, path: &str, len: u64) -> bool {
        let p = self.write_path(path);
        std::fs::OpenOptions::new().write(true).open(p).and_then(|f| f.set_len(len)).is_ok()
    }

    fn mkdir(&mut self, path: &str) -> bool {
        if self.exists(path) || (path.contains('\\') && !self.is_dir(parent(path))) {
            return false;
        }
        let p = self.write_path(path);
        std::fs::create_dir(&p).is_ok() || p.is_dir()
    }

    fn remove(&mut self, path: &str) -> bool {
        let Some(m) = self.stat(path) else { return false };
        if m.is_dir && !self.list(path).is_empty() {
            return false;
        }
        let w = self.write_path(path);
        let _ = if m.is_dir { std::fs::remove_dir(&w) } else { std::fs::remove_file(&w) };
        if self.overlay.is_some() {
            self.deleted.insert(path.to_lowercase());
        }
        true
    }

    fn rename(&mut self, from: &str, to: &str) -> bool {
        if !self.exists(from) {
            return false;
        }
        let a = self.write_path(from);
        let b = self.write_path(to);
        let ok = std::fs::rename(a, b).is_ok();
        if ok && self.overlay.is_some() {
            self.deleted.insert(from.to_lowercase());
        }
        ok
    }
}

// ---- in-memory -------------------------------------------------------------------------------

#[derive(Clone)]
struct Node {
    /// Name as first created (case preserved).
    name: String,
    is_dir: bool,
    data: Vec<u8>,
    mtime: u64,
}

#[derive(Default)]
pub struct MemFs {
    /// Lowercased full path -> node.
    nodes: BTreeMap<String, Node>,
    /// FILETIME stamped on writes.
    pub now: u64,
    /// Paths written since the last `take_dirty` (for persistence).
    dirty: HashSet<String>,
}

impl MemFs {
    pub fn new() -> Self {
        MemFs::default()
    }

    /// Adds a file (and its parent directories), e.g. from the game's download.
    pub fn insert(&mut self, path: &str, data: Vec<u8>) {
        let now = self.now;
        self.insert_with_time(path, data, now);
    }

    /// Adds a file with a given last-write time (FILETIME).
    pub fn insert_with_time(&mut self, path: &str, data: Vec<u8>, mtime: u64) {
        self.ensure_parents(path);
        let name = path.rsplit('\\').next().unwrap_or(path).to_string();
        self.nodes.insert(path.to_lowercase(), Node { name, is_dir: false, data, mtime });
    }

    /// A folder (and its parents) the game comes with.
    pub fn insert_dir(&mut self, path: &str) {
        let path = path.trim_end_matches('\\');
        self.ensure_parents(path);
        self.mkdir(path);
    }

    fn ensure_parents(&mut self, path: &str) {
        let mut acc = String::new();
        let comps: Vec<&str> = path.split('\\').filter(|c| !c.is_empty()).collect();
        for c in comps.iter().take(comps.len().saturating_sub(1)) {
            if !acc.is_empty() {
                acc.push('\\');
            }
            acc.push_str(c);
            self.nodes.entry(acc.to_lowercase()).or_insert_with(|| Node { name: c.to_string(), is_dir: true, data: Vec::new(), mtime: 0 });
        }
    }

    /// Paths changed since the last call.
    pub fn take_dirty(&mut self) -> Vec<String> {
        let mut v: Vec<String> = self.dirty.drain().collect();
        v.sort();
        v
    }

    /// The path of a key with each component as it was created (keys are lowercase).
    fn stored_path(&self, key: &str) -> String {
        let mut acc = String::new();
        let mut out = Vec::new();
        for c in key.split('\\') {
            if !acc.is_empty() {
                acc.push('\\');
            }
            acc.push_str(c);
            out.push(self.nodes.get(&acc).map(|n| n.name.clone()).unwrap_or_else(|| c.to_string()));
        }
        out.join("\\")
    }

    fn touch(&mut self, key: &str) {
        self.dirty.insert(key.to_string());
    }
}

impl Vfs for MemFs {
    fn take_changes(&mut self) -> Vec<(String, Option<(Vec<u8>, u64)>)> {
        let mut out = Vec::new();
        for key in self.take_dirty() {
            match self.nodes.get(&key) {
                Some(n) if n.is_dir => {}
                Some(n) => out.push((self.stored_path(&key), Some((n.data.clone(), n.mtime)))),
                None => out.push((self.stored_path(&key), None)),
            }
        }
        out
    }

    fn stat(&self, path: &str) -> Option<Meta> {
        if path.is_empty() {
            return Some(Meta { is_dir: true, size: 0, mtime: 0 });
        }
        self.nodes.get(&path.to_lowercase()).map(|n| Meta { is_dir: n.is_dir, size: n.data.len() as u64, mtime: n.mtime })
    }

    fn list(&self, dir: &str) -> Vec<(String, Meta)> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{}\\", dir.to_lowercase()) };
        self.nodes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter(|(k, _)| !k[prefix.len()..].contains('\\'))
            .map(|(_, n)| (n.name.clone(), Meta { is_dir: n.is_dir, size: n.data.len() as u64, mtime: n.mtime }))
            .collect()
    }

    fn read_at(&self, path: &str, pos: u64, buf: &mut [u8]) -> Option<usize> {
        let n = self.nodes.get(&path.to_lowercase())?;
        if n.is_dir {
            return None;
        }
        let start = (pos as usize).min(n.data.len());
        let k = buf.len().min(n.data.len() - start);
        buf[..k].copy_from_slice(&n.data[start..start + k]);
        Some(k)
    }

    fn write_at(&mut self, path: &str, pos: u64, data: &[u8]) -> bool {
        let key = path.to_lowercase();
        if !self.nodes.contains_key(&key) {
            self.insert(path, Vec::new());
        }
        let now = self.now;
        let n = self.nodes.get_mut(&key).unwrap();
        if n.is_dir {
            return false;
        }
        let end = pos as usize + data.len();
        if n.data.len() < end {
            n.data.resize(end, 0);
        }
        n.data[pos as usize..end].copy_from_slice(data);
        n.mtime = now;
        self.touch(&key);
        true
    }

    fn create(&mut self, path: &str) -> bool {
        if self.is_dir(path) && !path.is_empty() {
            return false;
        }
        if !path.contains('\\') || self.is_dir(parent(path)) {
            self.insert(path, Vec::new());
            self.touch(&path.to_lowercase());
            true
        } else {
            false
        }
    }

    fn set_len(&mut self, path: &str, len: u64) -> bool {
        let key = path.to_lowercase();
        match self.nodes.get_mut(&key) {
            Some(n) if !n.is_dir => {
                n.data.resize(len as usize, 0);
                self.touch(&key);
                true
            }
            _ => false,
        }
    }

    fn mkdir(&mut self, path: &str) -> bool {
        let key = path.to_lowercase();
        if self.nodes.contains_key(&key) || (path.contains('\\') && !self.is_dir(parent(path))) {
            return false;
        }
        let name = path.rsplit('\\').next().unwrap_or(path).to_string();
        self.nodes.insert(key.clone(), Node { name, is_dir: true, data: Vec::new(), mtime: self.now });
        self.touch(&key);
        true
    }

    fn remove(&mut self, path: &str) -> bool {
        let key = path.to_lowercase();
        match self.nodes.get(&key) {
            Some(n) if n.is_dir && !self.list(path).is_empty() => false,
            Some(_) => {
                self.nodes.remove(&key);
                self.touch(&key);
                true
            }
            None => false,
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> bool {
        let (fk, tk) = (from.to_lowercase(), to.to_lowercase());
        let Some(mut n) = self.nodes.remove(&fk) else { return false };
        n.name = to.rsplit('\\').next().unwrap_or(to).to_string();
        // Move a directory's contents along with it.
        let children: Vec<String> = self.nodes.keys().filter(|k| k.starts_with(&format!("{fk}\\"))).cloned().collect();
        for k in children {
            let c = self.nodes.remove(&k).unwrap();
            let nk = format!("{tk}{}", &k[fk.len()..]);
            self.touch(&k);
            self.touch(&nk);
            self.nodes.insert(nk, c);
        }
        self.nodes.insert(tk.clone(), n);
        self.touch(&fk);
        self.touch(&tk);
        true
    }
}
