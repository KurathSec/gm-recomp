//! Save states: the whole process (guest memory, registers, the emulated Windows state)
//! as bytes, and back. Serde helpers for the state's static references live here too.

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Mutex;

/// A `&'static str` read back: the same text interned once (names of kinds and fields,
/// a few dozen in all), so loading states again and again leaks nothing more.
pub fn intern(s: &str) -> &'static str {
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap();
    let set = names.get_or_insert_with(HashSet::new);
    if let Some(&n) = set.get(s) {
        return n;
    }
    let n: &'static str = Box::leak(s.to_owned().into_boxed_str());
    set.insert(n);
    n
}

/// A `&'static str` field spelled through this alias, so serde's derive does not take it
/// for a borrow of the input (it does for any field written `&str`, `with` or not).
pub type Name = &'static str;

/// `#[serde(with = "crate::state::static_str")]` for a [`Name`].
pub mod static_str {
    use super::*;
    pub fn serialize<S: Serializer>(v: &&'static str, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<&'static str, D::Error> {
        Ok(intern(&String::deserialize(d)?))
    }
}

/// `#[serde(with = "crate::state::static_keys")]` for a map keyed by `&'static str`.
pub mod static_keys {
    use super::*;
    use crate::fasthash::HashMap;
    pub fn serialize<S: Serializer, V: Serialize>(m: &HashMap<&'static str, V>, s: S) -> Result<S::Ok, S::Error> {
        let mut v: Vec<(&str, &V)> = m.iter().map(|(k, v)| (*k, v)).collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v.serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, V: Deserialize<'de>>(d: D) -> Result<HashMap<&'static str, V>, D::Error> {
        let v: Vec<(String, V)> = Vec::deserialize(d)?;
        Ok(v.into_iter().map(|(k, v)| (intern(&k), v)).collect())
    }
}

/// `#[serde(with = "crate::state::com_classes")]`: each COM object's interfaces, by name,
/// found again in `hle::com::class_of`.
pub mod com_classes {
    use super::*;
    use crate::fasthash::HashMap;
    use crate::hle::com::Interface;
    pub fn serialize<S: Serializer>(m: &HashMap<u32, &'static [&'static Interface]>, s: S) -> Result<S::Ok, S::Error> {
        let mut v: Vec<(u32, Vec<&str>)> = m.iter().map(|(k, c)| (*k, c.iter().map(|i| i.name).collect())).collect();
        v.sort_by_key(|e| e.0);
        v.serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<HashMap<u32, &'static [&'static Interface]>, D::Error> {
        let v: Vec<(u32, Vec<String>)> = Vec::deserialize(d)?;
        let mut out = HashMap::default();
        for (k, names) in v {
            let class = crate::hle::com::class_of(&names).ok_or_else(|| serde::de::Error::custom(format!("unknown COM class {names:?}")))?;
            out.insert(k, class);
        }
        Ok(out)
    }
}

/// `#[serde(with = "crate::state::big_array")]` for arrays longer than serde's 32.
pub mod big_array {
    use super::*;
    pub fn serialize<S: Serializer, T: Serialize, const N: usize>(a: &[T; N], s: S) -> Result<S::Ok, S::Error> {
        a.as_slice().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>, const N: usize>(d: D) -> Result<[T; N], D::Error> {
        let v: Vec<T> = Vec::deserialize(d)?;
        let n = v.len();
        v.try_into().map_err(|_| serde::de::Error::custom(format!("array of {n}, not {N}")))
    }
}

/// `#[serde(with = "crate::state::boxed_array")]`: `big_array` for a boxed array.
pub mod boxed_array {
    use super::*;
    pub fn serialize<S: Serializer, T: Serialize, const N: usize>(a: &Box<[T; N]>, s: S) -> Result<S::Ok, S::Error> {
        a.as_slice().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>, const N: usize>(d: D) -> Result<Box<[T; N]>, D::Error> {
        Ok(Box::new(big_array::deserialize(d)?))
    }
}

// ---- making and loading states ------------------------------------------------------------

use crate::engine::{self, Ctl, Machine, StateOp};
use crate::hle::RegistryState;
use crate::process::{Process, ThreadCtx};

/// Raised when the layout changes in a way `SCHEMA` does not see.
const FORMAT: u32 = 1;
/// A hash of every serialized type's definition (build.rs): a state is only read by a
/// runtime that reads it the same way.
pub const SCHEMA: &str = env!("GMR_STATE_SCHEMA");

/// Where the guest's temp files are: in a state, as the process may still read them
/// (GameMaker 8 unpacks extensions and included files there at the start).
const TEMP_DIRS: [&str; 2] = ["C\\Temp", "C\\Users\\player\\AppData\\Local\\Temp"];

/// Presents a load waits at most for the host frames to be what the state's were.
const LOAD_TRIES: u32 = 50;

#[derive(Serialize, Deserialize)]
struct Header {
    format: u32,
    schema: String,
    exe: String,
    shape: Shape,
}

/// The calls the host is in on the main thread's behalf (window procedures it runs,
/// GameMaker 8's game loop being in one): a state goes back into the same calls only,
/// whose host frames stay as they are.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Shape {
    depth: u32,
    tops: Vec<(u32, u32)>,
    nest: u32,
    frames64: Vec<u32>,
}

fn shape<M: Machine>(m: &M) -> Shape {
    let e = m.engine_ref();
    let p = e.process();
    Shape { depth: e.depth, tops: e.frame_tops.iter().map(|&(t, top)| (t as u32, top)).collect(), nest: p.nest, frames64: p.host_frames64.iter().map(|f| f.0).collect() }
}

#[derive(Serialize)]
struct BodyOut<'a> {
    ctx: &'a ThreadCtx,
    tsc: u64,
    registry: RegistryState,
    temp: Vec<(String, Vec<u8>)>,
    ranges: &'a [(u32, u32)],
    process: &'a Process,
}

#[derive(Deserialize)]
struct BodyIn {
    ctx: ThreadCtx,
    tsc: u64,
    registry: RegistryState,
    temp: Vec<(String, Vec<u8>)>,
    ranges: Vec<(u32, u32)>,
    process: Process,
}

enum Outcome {
    Done(String),
    /// Not here: try at the next frame.
    Later(String),
}

/// From `engine::call_api` once API `id` returned, while a `StateOp` waits: does it at
/// the end of a frame (a Present) on the main thread. Its state is then the registers,
/// guest memory and the process (the other threads wait with theirs saved), and the
/// host frames of the calls it is in, which are the same at every frame.
pub fn at_api_return<M: Machine>(m: &mut M, id: crate::hle::ApiId) {
    let e = m.engine_ref();
    let Some(p) = e.p.as_ref() else { return };
    if p.current != 0 || p.in_scheduler || e.ctl.is_some() || !p.registry.entries.get(id.0 as usize).is_some_and(|a| a.name.ends_with("::Present")) {
        return;
    }
    let Some(op) = m.engine().state_op.take() else { return };
    let t = m.now_us();
    let r = match op {
        StateOp::Save => save(m),
        StateOp::Load => load(m),
    };
    let ms = (m.now_us() - t) as f64 / 1000.0;
    match r {
        Ok(Outcome::Done(what)) => {
            m.engine().state_tries = 0;
            m.print(&format!("save state: {what} in {ms:.0} ms"));
        }
        Ok(Outcome::Later(why)) => {
            let e = m.engine();
            e.state_tries += 1;
            if e.state_tries < LOAD_TRIES {
                e.state_op = Some(op);
            } else {
                e.state_tries = 0;
                m.print(&format!("save state: not loaded: {why}"));
                m.state_failed("the game is not at a point it can go back from (try again once it plays)");
            }
        }
        Err(e) => {
            m.engine().state_tries = 0;
            m.print(&format!("save state: {e}"));
            m.state_failed(&e);
        }
    }
}

/// What a state holds of guest memory: the committed regions, the areas the process
/// lays out itself (the main thread's TEB, x87 page and PEB are not regions), each
/// thread's TEB and stack and each module's image; not the resolve cache, which names
/// this session's translations.
pub fn memory_ranges(p: &Process) -> Vec<(u32, u32)> {
    use crate::process::{PEB_BASE, RET_TO_HOST, STACK_BASE, STACK_SIZE, STUB_BASE, TEB_BASE};
    let mut spans: Vec<(u64, u64)> = Vec::new();
    let mut add = |a: u32, n: u64| {
        if n > 0 {
            spans.push((a as u64, a as u64 + n));
        }
    };
    for (&base, r) in &p.vm.regions {
        if r.committed {
            add(base, r.size as u64);
        }
    }
    for (base, size) in [(TEB_BASE - 0x1000, 0x2000u32), (PEB_BASE, 0x1000), (STACK_BASE, STACK_SIZE), (RET_TO_HOST, 0x1000), (STUB_BASE, 0x0010_0000)] {
        add(base, size as u64);
    }
    for t in &p.threads {
        add(t.teb.saturating_sub(0x1000), 0x3000);
        add(t.stack_limit, t.stack_base.saturating_sub(t.stack_limit) as u64);
    }
    for md in &p.modules {
        add(md.base, md.size as u64);
    }
    spans.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (a, b) in spans {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let cache = (recomp::abi::RESOLVE_CACHE as u64, recomp::abi::RESOLVE_CACHE as u64 + recomp::abi::RESOLVE_CACHE_SIZE as u64);
    let mut out = Vec::new();
    for (a, b) in merged {
        for (a, b) in [(a, b.min(cache.0)), (a.max(cache.1), b)] {
            if a < b {
                out.push((a as u32, (b - a) as u32));
            }
        }
    }
    out
}

/// Everything the guest ever had of memory (`Vm::used`): a loaded state zeroes it before
/// its own memory goes in, as memory outside `used` must be zero.
fn used_ranges(p: &Process) -> Vec<(u32, u32)> {
    p.vm.used.iter().filter(|&(&a, &e)| e > a as u64).map(|(&a, &e)| (a, (e - a as u64).min(u32::MAX as u64) as u32)).collect()
}

fn temp_files(fs: &dyn crate::vfs::Vfs) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut dirs: Vec<String> = TEMP_DIRS.iter().map(|d| d.to_string()).collect();
    while let Some(dir) = dirs.pop() {
        for (name, meta) in fs.list(&dir) {
            let path = format!("{dir}\\{name}");
            if meta.is_dir {
                dirs.push(path);
            } else if let Some(data) = fs.read_all(&path) {
                out.push((path, data));
            }
        }
    }
    out
}

fn save<M: Machine>(m: &mut M) -> Result<Outcome, String> {
    let ctx = engine::save_ctx(m);
    let shape = shape(m);
    let (meta, ranges) = {
        let p = m.engine_ref().process();
        let ranges = memory_ranges(p);
        let header = Header { format: FORMAT, schema: SCHEMA.to_string(), exe: p.exe_path.clone(), shape };
        let body = BodyOut { ctx: &ctx, tsc: m.engine_ref().tsc, registry: p.registry.state(), temp: temp_files(p.fs.as_ref()), ranges: &ranges, process: p };
        let mut meta = postcard::to_stdvec(&header).map_err(|e| e.to_string())?;
        meta.extend(postcard::to_stdvec(&body).map_err(|e| format!("cannot write the process: {e}"))?);
        (meta, ranges)
    };
    let mem: u64 = ranges.iter().map(|r| r.1 as u64).sum();
    m.state_saved(&meta, &ranges);
    Ok(Outcome::Done(format!("saved ({} KB of state, {} MB of memory in {} ranges)", meta.len() / 1024, mem >> 20, ranges.len())))
}

fn load<M: Machine>(m: &mut M) -> Result<Outcome, String> {
    let meta = m.state_meta().ok_or("no state to load")?;
    let (h, rest) = postcard::take_from_bytes::<Header>(&meta).map_err(|_| "not a save state".to_string())?;
    if h.format != FORMAT || h.schema != SCHEMA {
        return Err("the state was made by another version of the player".into());
    }
    let exe = m.engine_ref().process().exe_path.clone();
    if h.exe != exe {
        return Err(format!("the state is of {}, not {exe}", h.exe));
    }
    let here = shape(m);
    if h.shape != here {
        return Ok(Outcome::Later(format!("the host frames are {here:?}, the state's {:?}", h.shape)));
    }
    let body: BodyIn = postcard::from_bytes(rest).map_err(|e| format!("the state cannot be read: {e}"))?;
    m.engine_ref().process().registry.check(&body.registry)?;
    let mut live = m.engine().p.take().ok_or("the process is lent out")?;
    let mut new = body.process;
    let keep = kept_textures(&live, &new);
    // From here on the state replaces the process; a failure stops it.
    drop_gpu_objects(&mut live, &keep);
    for &(a, n) in &body.ranges {
        if m.ensure_mapped(a, n).is_err() {
            m.engine().p = Some(live);
            engine::stop(m, format!("save state: cannot map {a:#x}+{n:#x}"));
            return Err("loading failed".into());
        }
    }
    if let Err(e) = m.state_memory(&used_ranges(&live)) {
        m.engine().p = Some(live);
        engine::stop(m, format!("save state: {e}"));
        return Err("loading failed".into());
    }
    new.adopt_host_parts(&mut live);
    if let Err(e) = new.registry.restore(&body.registry) {
        m.engine().p = Some(Box::new(new));
        engine::stop(m, format!("save state: {e}"));
        return Err("loading failed".into());
    }
    // modules' file bytes: the running process's copy of the same image, or the file
    for md in &mut new.modules {
        let same = live.modules.iter().find(|l| l.base == md.base && l.size == md.size && l.name.eq_ignore_ascii_case(&md.name));
        md.file = match same {
            Some(l) => l.file.clone(),
            None => new.fs.read_all(&live.vpath(&md.path)).unwrap_or_default(),
        };
    }
    let bases: Vec<u32> = new.modules.iter().map(|md| md.base).collect();
    new.resources.retain(|base, _| bases.contains(base));
    for (path, data) in &body.temp {
        new.fs.create(path);
        new.fs.write_at(path, 0, data);
    }
    carry_input(&mut new, &live);
    crate::hle::mci::reopen(&mut new);
    new.dm_voices.clear();
    new.mixer = crate::hle::audio::Mixer::default();
    new.mixer.base = new.mixer.frame_at(new.clock_us);
    make_gpu_objects(&mut new, m, &keep);
    drop(live);
    m.engine().p = Some(Box::new(new));
    let _ = m.fill(recomp::abi::RESOLVE_CACHE, recomp::abi::RESOLVE_CACHE_SIZE, 0);
    resync_engine(m);
    m.engine().tsc = body.tsc;
    engine::restore_ctx(m, &body.ctx);
    engine::request(m, Ctl::Resume);
    m.state_loaded();
    let mem: u64 = body.ranges.iter().map(|r| r.1 as u64).sum();
    Ok(Outcome::Done(format!("loaded ({} MB of memory)", mem >> 20)))
}

/// Input as the player has it now, not as it was when the state was made: keys held then
/// and not now are released and the other way round (as messages, which games that keep
/// their own key state need), and the mouse and controllers are as they are.
fn carry_input(new: &mut Process, live: &Process) {
    use crate::hle::InputEvent;
    for vk in 0..=255u8 {
        // (the mouse buttons go with the mouse)
        if matches!(vk, 0x01 | 0x02 | 0x04) {
            continue;
        }
        let (was, is) = (new.key_state[vk as usize] & 0x80 != 0, live.key_state[vk as usize] & 0x80 != 0);
        if was != is {
            new.injected_input.push(InputEvent::Key { vk, scan: 0, down: is });
        }
    }
    new.dik = live.dik;
    if new.mouse != live.mouse || new.mouse_buttons != live.mouse_buttons {
        new.injected_input.push(InputEvent::Mouse { x: live.mouse.0, y: live.mouse.1, buttons: live.mouse_buttons });
    }
    for (i, (pad, was)) in live.pads.iter().zip(&new.pads).enumerate() {
        if (pad.connected, pad.buttons, pad.triggers, pad.thumbs) != (was.connected, was.buttons, was.triggers, was.thumbs) {
            new.injected_input.push(InputEvent::Pad { index: i as u8, connected: pad.connected, buttons: pad.buttons, triggers: pad.triggers, thumbs: pad.thumbs });
        }
    }
}

/// The engine after the process changed under it: modules that are not the state's lose
/// their translations (code there is something else now, or nothing), the state's new
/// ones are discovered, and what was learned of the code that went is dropped.
fn resync_engine<M: Machine>(m: &mut M) {
    let e = m.engine();
    let Some(p) = e.p.as_ref() else { return };
    let keep: Vec<(u32, u32, String)> = p.modules.iter().map(|md| (md.base, md.size, md.name.to_ascii_lowercase())).collect();
    let written = p.code_written.len();
    let gone: Vec<(u32, u32)> = e
        .mods
        .iter()
        .filter(|k| !keep.iter().any(|(b, s, n)| *b == k.base && *s == k.size && *n == k.name.to_ascii_lowercase()))
        .map(|k| (k.base, k.size))
        .collect();
    if !gone.is_empty() {
        let inside = |a: u32| gone.iter().any(|&(b, s)| a >= b && (a as u64) < b as u64 + s as u64);
        e.funcs.retain(|&a, _| !inside(a));
        e.mods.retain(|k| !gone.contains(&(k.base, k.size)));
        e.idle_queue = None;
        e.idle_next.clear();
        e.inline_cache = None;
    }
    e.code_written_seen = written;
    engine::sync_modules(m);
}

/// Whether texture object `o` is a render target (its contents exist only on the GPU).
fn render_target(o: &crate::process::D3dObject) -> bool {
    let f = |k: &str| o.data.get(k).copied().unwrap_or(0);
    o.kind.contains("Surface") || if o.kind.starts_with("ID3D11") { f("bind") & 0x20 != 0 } else { f("usage") & 1 != 0 }
}

/// Textures whose GPU copy a loaded state keeps instead of uploading them again (a large
/// game's texture pages took seconds of uneven frames after every load): the same texture
/// in both processes (kind, size, format, levels), written last by the same write (the
/// content stamp, `Process::stamp_content`), nothing of it waiting to be uploaded in
/// either, and not a render target.
fn kept_textures(live: &Process, new: &Process) -> HashSet<u32> {
    let f = |o: &crate::process::D3dObject, k: &str| o.data.get(k).copied().unwrap_or(0);
    new.d3d_objects
        .iter()
        .filter(|&(id, o)| {
            let Some(l) = live.d3d_objects.get(id) else { return false };
            f(o, "gpu") != 0
                && f(l, "gpu") != 0
                && o.kind == l.kind
                && !render_target(o)
                && f(o, "dirty") == 0
                && f(l, "dirty") == 0
                && (f(o, "rev_lo") | f(o, "rev_hi")) != 0
                && ["w", "h", "fmt", "levels", "rev_lo", "rev_hi"].iter().all(|k| f(o, k) == f(l, k))
        })
        .map(|(&id, _)| id)
        .collect()
}

/// Deletes the host's copies of the process's textures (but those in `keep`) and shaders.
fn drop_gpu_objects(p: &mut Process, keep: &HashSet<u32>) {
    let ids: Vec<(u32, bool, bool)> = p.d3d_objects.iter().map(|(&id, o)| (id, o.data.get("gpu").copied().unwrap_or(0) != 0, o.data.get("shader").copied().unwrap_or(0) != 0)).collect();
    for (id, tex, shader) in ids {
        if tex && !keep.contains(&id) {
            p.gpu.texture_delete(id);
        }
        if shader || p.d3d11.shaders.contains_key(&id) {
            p.gpu.shader_delete(id);
        }
    }
}

/// Makes the host's textures and shaders of a loaded process: textures upload their
/// contents from guest memory when next drawn (render targets' only exist on the GPU
/// and start out empty); those in `keep` are there already.
fn make_gpu_objects(p: &mut Process, mem: &dyn crate::mem::Guest, keep: &HashSet<u32>) {
    let (bw, bh) = p.d3d.backbuffer;
    if bw != 0 && bh != 0 {
        p.gpu.resize(bw, bh);
    }
    let mut ids: Vec<u32> = p.d3d_objects.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let o = &p.d3d_objects[&id];
        let f = |k: &str| o.data.get(k).copied().unwrap_or(0);
        if f("gpu") != 0 && keep.contains(&id) {
            continue;
        }
        if f("gpu") != 0 {
            let levels = f("levels").clamp(1, 16);
            let rt = render_target(o);
            let version = if o.kind.ends_with('8') { 8 } else { 9 };
            p.gpu.texture_create(id, f("w"), f("h"), levels, f("fmt"), rt);
            let dirty = (1u32 << levels) - 1;
            if let Some(o) = p.d3d_objects.get_mut(&id) {
                o.data.insert("dirty", dirty);
            }
            if !p.pending_uploads.contains(&(id, version)) {
                p.pending_uploads.push((id, version));
            }
        } else if f("shader") != 0 {
            let vertex = o.kind.contains("Vertex");
            let (at, n) = (f("heap_bits"), f("tokens").min(65536));
            let mut tokens = vec![0u32; n as usize];
            for (i, t) in tokens.iter_mut().enumerate() {
                *t = mem.read_u32(at + 4 * i as u32).unwrap_or(0x0000_FFFF);
            }
            if let Ok(glsl) = crate::hle::d3d9_shader::translate(&tokens) {
                p.gpu.shader_create(id, vertex, &glsl);
            }
        }
    }
    let shaders: Vec<(u32, bool, String)> = p.d3d11.shaders.iter().map(|(&id, s)| (id, s.vertex, s.glsl.clone())).collect();
    for (id, vertex, glsl) in shaders {
        p.gpu.shader_create(id, vertex, &glsl);
    }
    let (id, w, h) = if p.d3d11.size != (0, 0) { p.d3d11.target } else { p.d3d.target };
    if w != 0 && h != 0 {
        p.gpu.set_target(id, w, h);
    }
}
