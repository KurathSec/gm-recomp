//! Mods for GameMaker games, applied to a game's files before it runs (the page's mod
//! options): patcher.rs's debug switch, and Fangame.ModLoader's mods reimplemented.
//!
//! The page hands over the files a mod may change (the entry exe, the data file) and the
//! mods' own files, then takes back the files that changed or were added.

pub mod debug;
pub mod gm8;
pub mod gm8x_fix;
mod gm8x_fix_data;
pub mod image;
pub mod mods;

use serde_json::{Value, json};

/// The engine fields of a game's manifest that decide what a mod can do.
#[derive(Clone, Debug, Default)]
pub struct Engine {
    /// "gm80", "gm81" or "gms"
    pub kind: String,
    pub bytecode: u32,
    pub yyc: bool,
    /// "data.win", "game.win", "embedded@<offset>" or empty
    pub data_file: String,
}

impl Engine {
    pub fn from_json(v: &Value) -> Engine {
        Engine {
            kind: v["kind"].as_str().unwrap_or("").to_owned(),
            bytecode: v["bytecode"].as_u64().unwrap_or(0) as u32,
            yyc: v["yyc"].as_bool().unwrap_or(false),
            data_file: v["data_file"].as_str().unwrap_or("").to_owned(),
        }
    }
    pub fn is_gm8(&self) -> bool {
        self.kind == "gm80" || self.kind == "gm81"
    }
    pub fn is_gms(&self) -> bool {
        self.kind == "gms"
    }
}

/// A game being modded: its files (paths as the page has them) and which changed.
pub struct Game {
    pub engine: Engine,
    pub entry: String,
    pub files: Vec<(String, Vec<u8>)>,
    pub changed: Vec<bool>,
}

fn same_path(a: &str, b: &str) -> bool {
    a.replace('\\', "/").eq_ignore_ascii_case(&b.replace('\\', "/"))
}

impl Game {
    pub fn new(engine: Engine, entry: &str) -> Game {
        Game { engine, entry: entry.to_owned(), files: Vec::new(), changed: Vec::new() }
    }
    pub fn add(&mut self, path: &str, data: Vec<u8>) {
        self.files.push((path.to_owned(), data));
        self.changed.push(false);
    }
    pub fn index(&self, path: &str) -> Option<usize> {
        self.files.iter().position(|(p, _)| same_path(p, path))
    }
    /// The file at `path` to change (marked changed).
    pub fn file_mut(&mut self, path: &str) -> Option<&mut Vec<u8>> {
        let i = self.index(path)?;
        self.changed[i] = true;
        Some(&mut self.files[i].1)
    }
    /// Adds or replaces a file.
    pub fn put(&mut self, path: &str, data: Vec<u8>) {
        match self.index(path) {
            Some(i) => {
                self.files[i].1 = data;
                self.changed[i] = true;
            }
            None => {
                self.files.push((path.to_owned(), data));
                self.changed.push(true);
            }
        }
    }
}

/// A mod's files (its assets), by name within the mod.
#[derive(Default)]
pub struct Assets {
    pub files: Vec<(String, String, Vec<u8>)>,
}

impl Assets {
    pub fn get(&self, module: &str, name: &str) -> Option<&[u8]> {
        self.files.iter().find(|(m, n, _)| m == module && same_path(n, name)).map(|(_, _, d)| d.as_slice())
    }
}

/// Applies the mods `mods` ([{id, config}], in order) to `game`; returns a log line per
/// change (or skip), or the first error.
pub fn apply(game: &mut Game, mods: &[Value], assets: &Assets) -> Result<Vec<String>, String> {
    let mut log = Vec::new();
    // GameMaker 8's game data, parsed once for all the mods and written back at the end
    let mut gm8: Option<gm8::Gm8> = None;
    // changes to the exe itself, after the game data is written (ModLoader's ModExecutable)
    let mut exe_mods: Vec<(String, &Value)> = Vec::new();
    for m in mods {
        let id = m["id"].as_str().unwrap_or("");
        let config = &m["config"];
        let lines = if id == "debug" {
            debug_mod(game, config)?
        } else if game.engine.is_gm8() {
            if gm8.is_none() {
                let exe = game.files.get(game.index(&game.entry).ok_or("the game's exe is missing")?).unwrap();
                gm8 = Some(gm8::Gm8::parse(&exe.1)?);
            }
            let g = gm8.as_mut().unwrap();
            match id {
                // (parsed and written back unchanged: a test)
                "_roundtrip" => Vec::new(),
                // (the assets' names: a test)
                "_list" => {
                    let names = |l: &gm8::List| -> Result<String, String> {
                        Ok(l.names()?.iter().flatten().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>().join(" "))
                    };
                    vec![format!("sprites: {}", names(&g.sprites)?), format!("objects: {}", names(&g.objects)?), format!("scripts: {}", names(&g.scripts)?)]
                }
                "GodMode" => mods::godmode_gm8(g)?,
                "AllowFullscreen" => mods::allow_fullscreen_gm8(g)?,
                "CrimsonKid" => mods::crimson_kid_gm8(g, config, assets)?,
                "DotKid" => mods::dot_kid_gm8(g, config, assets)?,
                "PlayOnline" => {
                    let i = game.index(&game.entry).ok_or("the game's exe is missing")?;
                    let (lines, files) = mods::play_online_gm8(g, config, assets, &game.files[i].1, &game.entry)?;
                    // (its files go next to the exe)
                    let dir = game.entry.rfind(['/', '\\']).map_or(String::new(), |k| game.entry[..=k].to_owned());
                    for (name, data) in files {
                        game.put(&format!("{dir}{name}"), data);
                    }
                    lines
                }
                "DbgHelper" => {
                    exe_mods.push((id.to_owned(), config));
                    Vec::new()
                }
                _ => return Err(format!("{id} does not work with GameMaker 8 games yet")),
            }
        } else {
            return Err(format!("{id} does not work with this game's engine yet"));
        };
        log.extend(lines.into_iter().map(|l| format!("{id}: {l}")));
    }
    let entry = game.entry.clone();
    if let Some(g) = gm8 {
        game.put(&entry, g.write());
    }
    for (id, config) in exe_mods {
        let exe = game.file_mut(&entry).ok_or("the game's exe is missing")?;
        let flag = |k: &str| config[k].as_bool().unwrap_or(true);
        let o = gm8x_fix::Options { input_lag: flag("InputLagPatch"), joystick: flag("JoystickPatch"), scheduler: flag("SchedulerPatch"), memory: flag("MemoryPatch"), direct_play: flag("DirectPlayPatch") };
        log.extend(gm8x_fix::apply(exe, &o)?.into_iter().map(|l| format!("{id}: {l}")));
    }
    Ok(log)
}

fn debug_mod(game: &mut Game, config: &Value) -> Result<Vec<String>, String> {
    let e = game.engine.clone();
    if !e.is_gms() {
        return Err("the debug switch is for GameMaker: Studio games".into());
    }
    if e.yyc {
        let entry = game.entry.clone();
        let exe = game.file_mut(&entry).ok_or("the game's exe is missing")?;
        return debug::patch_yyc(exe);
    }
    let mut names = debug::VM_NAMES.to_vec();
    if config["noDeath"].as_bool().unwrap_or(false) {
        names.push(debug::VM_NO_DEATH);
    }
    if let Some(off) = e.data_file.strip_prefix("embedded@") {
        // the data file inside the exe
        let off: usize = off.parse().map_err(|_| "bad embedded offset")?;
        let entry = game.entry.clone();
        let exe = game.file_mut(&entry).ok_or("the game's exe is missing")?;
        let form = exe.get_mut(off..).ok_or("embedded data past the end of the exe")?;
        return debug::patch_vm(form, &names);
    }
    let data = game.file_mut(&e.data_file).ok_or_else(|| format!("{} is missing", e.data_file))?;
    debug::patch_vm(data, &names)
}

// ---- exports for the page (one game at a time) ---------------------------------------------

struct State {
    game: Option<Game>,
    assets: Assets,
    mods: Vec<Value>,
    out: Vec<u8>,
}

static mut STATE: State = State { game: None, assets: Assets { files: Vec::new() }, mods: Vec::new(), out: Vec::new() };

#[allow(static_mut_refs)]
fn state() -> &'static mut State {
    // Single-threaded: the page's one instance.
    unsafe { &mut STATE }
}

fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(ptr, len) } }
}

fn text(ptr: *const u8, len: usize) -> String {
    String::from_utf8_lossy(bytes(ptr, len)).into_owned()
}

/// `len` bytes for the page to fill (freed with `dealloc`).
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut v = vec![0u8; len.max(1)];
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[unsafe(no_mangle)]
pub extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    unsafe { drop(Vec::from_raw_parts(ptr, len.max(1), len.max(1))) };
}

/// Starts a game: `json` is {engine (the manifest's), entry, mods: [{id, config}]}.
#[unsafe(no_mangle)]
pub extern "C" fn begin(ptr: *const u8, len: usize) -> u32 {
    let st = state();
    let Ok(v) = serde_json::from_slice::<Value>(bytes(ptr, len)) else { return 1 };
    st.game = Some(Game::new(Engine::from_json(&v["engine"]), v["entry"].as_str().unwrap_or("")));
    st.mods = v["mods"].as_array().cloned().unwrap_or_default();
    st.assets = Assets::default();
    0
}

/// One of the game's files (the bytes are copied).
#[unsafe(no_mangle)]
pub extern "C" fn add_file(path: *const u8, path_len: usize, ptr: *const u8, len: usize) {
    if let Some(g) = state().game.as_mut() {
        g.add(&text(path, path_len), bytes(ptr, len).to_vec());
    }
}

/// One of mod `module`'s files.
#[unsafe(no_mangle)]
pub extern "C" fn add_asset(module: *const u8, module_len: usize, name: *const u8, name_len: usize, ptr: *const u8, len: usize) {
    state().assets.files.push((text(module, module_len), text(name, name_len), bytes(ptr, len).to_vec()));
}

/// Applies the mods; the output is {log: [...]} or {error}. Returns 0, or 1 on an error.
#[unsafe(no_mangle)]
pub extern "C" fn run() -> u32 {
    let st = state();
    let Some(game) = st.game.as_mut() else { return 1 };
    let (r, code) = match apply(game, &st.mods, &st.assets) {
        Ok(log) => (json!({ "log": log }), 0),
        Err(e) => (json!({ "error": e }), 1),
    };
    st.out = r.to_string().into_bytes();
    code
}

/// The files that changed or were added: their count, then each one's path (as the
/// output), bytes and length.
#[unsafe(no_mangle)]
pub extern "C" fn changed_count() -> u32 {
    state().game.as_ref().map_or(0, |g| g.changed.iter().filter(|&&c| c).count() as u32)
}

fn changed_nth(i: u32) -> Option<&'static (String, Vec<u8>)> {
    let g = state().game.as_ref()?;
    g.files.iter().zip(&g.changed).filter(|(_, c)| **c).map(|(f, _)| f).nth(i as usize)
}

#[unsafe(no_mangle)]
pub extern "C" fn changed_path(i: u32) {
    state().out = changed_nth(i).map(|f| f.0.clone().into_bytes()).unwrap_or_default();
}

#[unsafe(no_mangle)]
pub extern "C" fn changed_ptr(i: u32) -> *const u8 {
    changed_nth(i).map_or(std::ptr::null(), |f| f.1.as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn changed_len(i: u32) -> usize {
    changed_nth(i).map_or(0, |f| f.1.len())
}

#[unsafe(no_mangle)]
pub extern "C" fn out_ptr() -> *const u8 {
    state().out.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn out_len() -> usize {
    state().out.len()
}

/// Drops the game (its memory goes back for the next one).
#[unsafe(no_mangle)]
pub extern "C" fn finish() {
    let st = state();
    st.game = None;
    st.assets = Assets::default();
    st.mods.clear();
    st.out = Vec::new();
}
