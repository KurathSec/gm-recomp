//! `harness run`: executes a game under wasmtime with the HLE runtime. The engine
//! (`runtime::engine`) does dispatching, API calls and translation; this file supplies
//! the wasmtime machine: guest memory, CPU-state globals, the function table and the
//! runtime services translated code imports.

use anyhow::{Context, Result, anyhow};
use recomp::abi::{self, GLOBALS, Global as G, RT_FUNCS};
use runtime::engine::{self, Engine, Machine};
use runtime::mem::Fault;
use runtime::{Guest, Process};
use std::path::PathBuf;
use std::time::Instant;
use wasmtime::{
    AsContext, AsContextMut, Caller, Config, Extern, Func, Global, GlobalType, Linker, Memory, MemoryType, Module, Mutability,
    Ref, RefType, Store, Table, TableType, Val, ValType,
};

/// 2 GiB of guest address space: everything the runtime places lives below 0x8000_0000.
const MEM_PAGES: u64 = 32768;
/// What it can grow to: a large-address-aware exe's 4 GiB.
const MEM_MAX_PAGES: u64 = 65536;

pub struct RunOpts {
    pub game_dir: PathBuf,
    pub exe: Option<String>,
    pub trace: bool,
    pub frames: u64,
    pub aot: bool,
    pub max_api_calls: u64,
    pub quiet: bool,
    /// Wall-clock limit in seconds (guest loops without API calls are interrupted).
    pub seconds: u64,
    /// Print this many of the last dispatcher targets when the run stops.
    pub recent: usize,
    /// Memory ranges (addr, len) to hex-dump when the run stops.
    pub dump: Vec<(u32, u32)>,
    /// Keep files written by earlier runs (saves) instead of starting clean.
    pub keep_writes: bool,
    pub write_dir: Option<PathBuf>,
    /// Write the whole guest memory to this file when the run stops.
    pub dump_mem: Option<PathBuf>,
    /// Scripted key events: (after frame, virtual-key code, pressed).
    pub keys: Vec<(u64, u8, bool, u16)>,
    /// Scripted mouse state: (after frame, x, y, buttons).
    pub mouse: Vec<(u64, i32, i32, u32)>,
    /// Write the mixed sound output to this WAV file.
    pub audio_out: Option<PathBuf>,
    /// A session recorded by the page's monitor mode, replayed.
    pub replay: Option<PathBuf>,
    pub epoch: Option<u64>,
}

/// What a monitor log holds for a replay.
#[derive(Default)]
struct Session {
    epoch: Option<u64>,
    /// (path in the game's file system, contents), None when the save deleted it.
    saves: Vec<(String, Option<Vec<u8>>)>,
    /// (frame, guest clock, event) as the guest took them.
    input: std::collections::VecDeque<(u64, u64, runtime::InputEvent)>,
}

fn read_session(path: &std::path::Path) -> Result<Session> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut s = Session::default();
    let num = |t: &str| -> i64 {
        if let Some(h) = t.strip_prefix("0x") { i64::from_str_radix(h, 16).unwrap_or(0) } else { t.parse().unwrap_or(0) }
    };
    for line in text.lines() {
        let w: Vec<&str> = line.split(' ').collect();
        match w.first().copied() {
            Some("start") => {
                s.epoch = w.iter().find_map(|t| t.strip_prefix("epoch=")).and_then(|v| v.parse().ok());
            }
            Some("save") if w.len() >= 2 => {
                let hex = w.get(2).copied().unwrap_or("");
                let data = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0)).collect();
                s.saves.push((w[1].replace("%20", " "), Some(data)));
            }
            Some("deleted") if w.len() >= 2 => s.saves.push((w[1].replace("%20", " "), None)),
            Some("input") if w.len() >= 5 => {
                let (frame, clock) = (num(w[1]) as u64, num(w[2]) as u64);
                let a: Vec<i64> = w[4..].iter().map(|t| num(t)).collect();
                let ev = match w[3] {
                    "key" if a.len() >= 3 => runtime::InputEvent::Key { vk: a[0] as u8, scan: a[1] as u16, down: a[2] != 0 },
                    "mouse" if a.len() >= 3 => runtime::InputEvent::Mouse { x: a[0] as i32, y: a[1] as i32, buttons: a[2] as u32 },
                    "pad" if a.len() >= 9 => runtime::InputEvent::Pad {
                        index: a[0] as u8,
                        connected: a[1] != 0,
                        buttons: a[2] as u16,
                        triggers: [a[3] as u8, a[4] as u8],
                        thumbs: [a[5] as i16, a[6] as i16, a[7] as i16, a[8] as i16],
                    },
                    _ => continue,
                };
                s.input.push_back((frame, clock, ev));
            }
            _ => {}
        }
    }
    Ok(s)
}

#[derive(Clone, Copy)]
struct Cpu {
    memory: Memory,
    table: Table,
    globals: [Global; abi::GLOBALS.len()],
}

/// Store data: the engine plus what instantiation needs.
struct Hd {
    engine: Engine,
    linker: Option<Linker<Hd>>,
    t0: Instant,
    /// Scripted input not delivered yet, by frame.
    keys: Vec<(u64, u8, bool, u16)>,
    mouse: Vec<(u64, i32, i32, u32)>,
    /// A recorded session's input, delivered at the poll it was taken at.
    replay: std::collections::VecDeque<(u64, u64, runtime::InputEvent)>,
    /// The MIDI SoundFont was handed over (or there is none).
    soundfont_given: bool,
    /// GMR_FRAME_MS=<ms>: report frames that took longer (host time between the
    /// game's input polls of consecutive frames) with what the engine did in them.
    frame_ms: Option<f64>,
    /// (frame, host µs, dispatches, x87 via rt.fpu, translated, translate µs, other
    /// threads µs) at the start of the frame being measured.
    frame_mark: (u64, u64, u64, u64, usize, u64, u64),
    /// The guest's network connections (GMR_NET).
    net: crate::net::Net,
    /// GMR_PROFILE=<ms>: translated functions sampled every <ms> ms (from the wasm stack
    /// at epoch ticks), as the innermost one and as anywhere on the stack.
    samples: Option<(std::collections::HashMap<String, u64>, std::collections::HashMap<String, u64>)>,
}

/// Store and Caller both give direct access to their data.
trait Cx: AsContextMut<Data = Hd> {
    fn hd(&mut self) -> &mut Hd;
    fn hd_ref(&self) -> &Hd;
}
impl Cx for Store<Hd> {
    fn hd(&mut self) -> &mut Hd {
        self.data_mut()
    }
    fn hd_ref(&self) -> &Hd {
        self.data()
    }
}
impl Cx for Caller<'_, Hd> {
    fn hd(&mut self) -> &mut Hd {
        self.data_mut()
    }
    fn hd_ref(&self) -> &Hd {
        self.data()
    }
}

struct Wt<'a, C: Cx> {
    cx: &'a mut C,
    cpu: Cpu,
}

impl<C: Cx> Guest for Wt<'_, C> {
    fn read(&self, addr: u32, buf: &mut [u8]) -> Result<(), Fault> {
        let data = self.cpu.memory.data(&*self.cx);
        match data.get(addr as usize..addr as usize + buf.len()) {
            Some(src) => {
                buf.copy_from_slice(src);
                Ok(())
            }
            None => Err(Fault { addr, write: false }),
        }
    }
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), Fault> {
        let mem = self.cpu.memory.data_mut(&mut *self.cx);
        match mem.get_mut(addr as usize..addr as usize + data.len()) {
            Some(dst) => {
                dst.copy_from_slice(data);
                Ok(())
            }
            None => Err(Fault { addr, write: true }),
        }
    }
    fn ensure_mapped(&mut self, addr: u32, size: u32) -> Result<(), Fault> {
        let end = addr as u64 + size as u64;
        let have = self.cpu.memory.data_size(&*self.cx) as u64;
        if end > have {
            let pages = (end - have).div_ceil(65536);
            self.cpu.memory.grow(&mut *self.cx, pages).map_err(|_| Fault { addr, write: true })?;
        }
        Ok(())
    }
}

impl<C: Cx> Machine for Wt<'_, C> {
    fn engine(&mut self) -> &mut Engine {
        &mut self.cx.hd().engine
    }
    fn engine_ref(&self) -> &Engine {
        &self.cx.hd_ref().engine
    }
    fn global(&mut self, g: G) -> u32 {
        self.cpu.globals[g as usize].get(&mut *self.cx).i32().unwrap_or(0) as u32
    }
    fn set_global(&mut self, g: G, v: u32) {
        let _ = self.cpu.globals[g as usize].set(&mut *self.cx, Val::I32(v as i32));
    }
    fn global64(&mut self, g: G) -> u64 {
        self.cpu.globals[g as usize].get(&mut *self.cx).i64().unwrap_or(0) as u64
    }
    fn set_global64(&mut self, g: G, v: u64) {
        let _ = self.cpu.globals[g as usize].set(&mut *self.cx, Val::I64(v as i64));
    }
    /// GMR_SYSTEM32's copy (the player bundle's system32/), as the page fetches it.
    fn system_dll(&mut self, name: &str) -> Option<Vec<u8>> {
        let dir = std::env::var("GMR_SYSTEM32").ok()?;
        let found = std::fs::read_dir(&dir).ok()?.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))?;
        std::fs::read(found.path()).ok()
    }
    fn soundfont(&mut self) -> Option<Vec<u8>> {
        // GMR_SOUNDFONT, else the system's General MIDI SoundFont; offered once
        let mut cx = self.cx.as_context_mut();
        let hd = cx.data_mut();
        if hd.soundfont_given {
            return None;
        }
        hd.soundfont_given = true;
        let path = std::env::var("GMR_SOUNDFONT").unwrap_or_else(|_| "/usr/share/soundfonts/default.sf2".into());
        std::fs::read(path).ok()
    }
    fn net_enabled(&mut self) -> bool {
        std::env::var_os("GMR_NET").is_some()
    }
    fn net_open(&mut self, id: u32, udp: bool, host: &str, port: u16) -> bool {
        self.cx.hd().net.open(id, udp, host, port)
    }
    fn net_send(&mut self, id: u32, data: &[u8]) -> bool {
        self.cx.hd().net.send(id, data)
    }
    fn net_recv(&mut self, id: u32, buf: &mut [u8]) -> Option<usize> {
        self.cx.hd().net.recv(id, buf)
    }
    fn net_available(&mut self, id: u32) -> u32 {
        self.cx.hd().net.available(id)
    }
    fn net_state(&mut self, id: u32) -> runtime::hle::winsock::NetState {
        self.cx.hd().net.state(id)
    }
    fn net_close(&mut self, id: u32) {
        self.cx.hd().net.close(id)
    }
    /// Nobody to ask: OK, Yes, or the default text. GMR_PROMPT=<text> answers a text
    /// prompt with that instead, and GMR_PROMPT=no answers a question No.
    fn prompt(&mut self, ask: runtime::hle::Ask, title: &str, text: &str) -> Option<String> {
        use runtime::hle::Ask;
        eprintln!("prompt [{title}]: {}", text.replace('\n', " | "));
        let given = std::env::var("GMR_PROMPT").ok();
        match ask {
            Ask::Message => Some(String::new()),
            Ask::Question | Ask::Error => (given.as_deref() != Some("no")).then(String::new),
            Ask::Text(d) => Some(given.unwrap_or_else(|| d.to_owned())),
        }
    }
    fn xmm(&mut self, i: u32) -> u128 {
        self.cpu.globals[abi::xmm_global(i) as usize].get(&mut *self.cx).v128().map(|v| v.as_u128()).unwrap_or(0)
    }
    fn set_xmm(&mut self, i: u32, v: u128) {
        let _ = self.cpu.globals[abi::xmm_global(i) as usize].set(&mut *self.cx, Val::V128(v.into()));
    }
    fn call_index(&mut self, idx: u32) -> Result<(), String> {
        let f = match self.cpu.table.get(&mut *self.cx, idx as u64) {
            Some(Ref::Func(Some(f))) => f,
            _ => return Err(format!("table slot {idx} is empty")),
        };
        // translated functions take the registers and return them (rax..r15 in a
        // 64-bit process)
        let (first, n) = if self.engine_ref().x64 { (G::Rax as usize, 16) } else { (0, 8) };
        let mut args = [Val::I32(0); 16];
        for (r, a) in args[..n].iter_mut().enumerate() {
            *a = self.cpu.globals[first + r].get(&mut *self.cx);
        }
        let mut res = [Val::I32(0); 16];
        f.call(&mut *self.cx, &args[..n], &mut res[..n]).map_err(|e| format!("{e:?}"))?;
        for (r, v) in res[..n].iter().enumerate() {
            self.cpu.globals[first + r].set(&mut *self.cx, *v).map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }
    fn instantiate(&mut self, wasm: &[u8], exports: &[String]) -> Result<Vec<u32>, String> {
        let module = Module::new(self.cx.as_context().engine(), wasm).map_err(|e| {
            let _ = std::fs::write("target/failed.wasm", wasm);
            format!("{e} (module written to target/failed.wasm)")
        })?;
        let linker = self.cx.hd_ref().linker.clone().ok_or("linker missing")?;
        let instance = linker.instantiate(&mut *self.cx, &module).map_err(|e| format!("instantiate: {e:?}"))?;
        let mut out = Vec::with_capacity(exports.len());
        for name in exports {
            let f = instance.get_func(&mut *self.cx, name).ok_or_else(|| format!("missing export {name}"))?;
            let idx = self.cpu.table.grow(&mut *self.cx, 1, Ref::Func(Some(f))).map_err(|e| format!("table grow: {e:?}"))?;
            out.push(idx as u32);
        }
        Ok(out)
    }
    fn table_copy(&mut self, dst: u32, src: u32) -> Result<(), String> {
        let f = self.cpu.table.get(&mut *self.cx, src as _).ok_or("no such table slot")?;
        self.cpu.table.set(&mut *self.cx, dst as _, f).map_err(|e| format!("table set: {e:?}"))
    }
    fn now_us(&self) -> u64 {
        self.cx.hd_ref().t0.elapsed().as_micros() as u64
    }
    fn print(&mut self, s: &str) {
        eprintln!("{s}");
    }
    fn take_input(&mut self, frame: u64, clock_us: u64) -> Vec<runtime::InputEvent> {
        if let Some(limit) = self.cx.hd_ref().frame_ms {
            let now = self.cx.hd_ref().t0.elapsed().as_micros() as u64;
            let hd = self.cx.hd();
            let e = &hd.engine;
            let mark = (frame, now, e.dispatches, e.fpu_ops, e.translated, e.translate_us, e.thread_us);
            let (f0, t, d, x, n, tu, th) = hd.frame_mark;
            let counting = e.count;
            if frame != f0 {
                let ms = (now - t) as f64 / 1000.0;
                hd.frame_mark = mark;
                let mut counts = String::new();
                if counting {
                    for (k, name) in abi::COUNT_NAMES.iter().enumerate() {
                        let a = abi::COUNTERS + 4 * k as u32;
                        let v = self.read_u32(a).unwrap_or(0);
                        let _ = self.write_u32(a, 0);
                        counts.push_str(&format!("  {name} {v}"));
                    }
                    // GMR_COUNT=top: the functions entered most this frame
                    if std::env::var("GMR_COUNT").is_ok_and(|v| v == "top" || v == "blocks") {
                        let mut slots = Vec::new();
                        for k in 0..abi::ENTRY_SLOTS {
                            let a = abi::ENTRY_COUNTS + 8 * k;
                            let (f, n) = (self.read_u32(a).unwrap_or(0), self.read_u32(a + 4).unwrap_or(0));
                            if n > 0 {
                                slots.push((n, f));
                                let _ = self.write_u32(a + 4, 0);
                            }
                        }
                        slots.sort_unstable_by(|a, b| b.cmp(a));
                        let top: Vec<String> = slots.iter().take(if std::env::var("GMR_COUNT").is_ok_and(|v| v == "blocks") { 400 } else { 12 }).map(|(n, f)| format!("{f:#x}:{n}")).collect();
                        counts.push_str(&format!("\n    top {}", top.join(" ")));
                    }
                }
                if ms > limit && f0 != 0 {
                    eprintln!(
                        "frame {f0}: {ms:.1} ms (other threads {:.1})  dispatches {}  x87 via rt.fpu {}  translated {} ({:.1} ms){counts}",
                        (mark.6 - th) as f64 / 1000.0,
                        mark.2 - d,
                        mark.3 - x,
                        mark.4 - n,
                        (mark.5 - tu) as f64 / 1000.0
                    );
                }
            }
        }
        let hd = self.cx.hd();
        let due: Vec<(u64, u8, bool, u16)> = hd.keys.iter().copied().filter(|k| k.0 <= frame).collect();
        hd.keys.retain(|k| k.0 > frame);
        let mice: Vec<(u64, i32, i32, u32)> = hd.mouse.iter().copied().filter(|k| k.0 <= frame).collect();
        hd.mouse.retain(|k| k.0 > frame);
        let mut out: Vec<runtime::InputEvent> = due.into_iter().map(|(_, vk, down, scan)| runtime::InputEvent::Key { vk, scan, down }).collect();
        out.extend(mice.into_iter().map(|(_, x, y, buttons)| runtime::InputEvent::Mouse { x, y, buttons }));
        while let Some(&(f, t, _)) = hd.replay.front() {
            if frame < f || frame == f && clock_us < t {
                break;
            }
            out.push(hd.replay.pop_front().unwrap().2);
        }
        out
    }
}

pub fn run(opts: RunOpts) -> Result<()> {
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(opts.game_dir.join("manifest.json")).context("reading manifest.json")?)?;
    let exe = match &opts.exe {
        Some(e) => e.clone(),
        None => manifest["entry"].as_str().ok_or_else(|| anyhow!("manifest has no entry"))?.to_string(),
    };
    let host_root = opts.game_dir.join("files");
    let exe_path = format!("C:\\game\\{exe}");
    let cmdline = format!("\"{exe_path}\"");

    let mut cfg = Config::new();
    cfg.wasm_tail_call(true);
    cfg.wasm_simd(true);
    cfg.wasm_bulk_memory(true);
    cfg.wasm_multi_value(true);
    // (guest recursion takes wasm stack: a GameMaker 8.1 game's deep script recursion,
    // which runs in the browser, overflowed wasmtime's 512 KB)
    cfg.async_stack_size(8 << 20);
    cfg.max_wasm_stack(4 << 20);
    if let Ok(cache) = wasmtime::Cache::from_file(None) {
        cfg.cache(Some(cache));
    }
    cfg.epoch_interruption(true);
    let wt_engine = wasmtime::Engine::new(&cfg).map_err(|e| anyhow!("{e:?}"))?;
    let profile_ms: Option<u64> = std::env::var("GMR_PROFILE").ok().and_then(|v| v.parse().ok());
    {
        let wt_engine = wt_engine.clone();
        let secs = opts.seconds;
        std::thread::spawn(move || match profile_ms {
            // (the ticks sample; the deadline callback keeps the wall-clock limit)
            Some(ms) => loop {
                std::thread::sleep(std::time::Duration::from_millis(ms.max(1)));
                wt_engine.increment_epoch();
            },
            None => {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                wt_engine.increment_epoch();
            }
        });
    }

    // Files the game writes go to a scratch layer, never into the imported game files.
    let slug = opts.game_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let write_root = opts.write_dir.clone().unwrap_or_else(|| PathBuf::from("target/gamedata").join(&slug));
    if !opts.keep_writes {
        let _ = std::fs::remove_dir_all(&write_root);
    }
    std::fs::create_dir_all(&write_root)?;
    let session = match &opts.replay {
        Some(p) => read_session(p)?,
        None => Session::default(),
    };
    // the saves the session started with, in the write layer
    for (path, data) in &session.saves {
        let mut host = write_root.clone();
        for comp in path.split(['\\', '/']).filter(|c| !c.is_empty()) {
            host.push(comp);
        }
        match data {
            Some(d) => {
                if let Some(parent) = host.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&host, d)?;
            }
            None => {}
        }
    }
    if opts.replay.is_some() {
        eprintln!("replay: {} saved files, {} input events, epoch {:?}", session.saves.len(), session.input.len(), session.epoch);
    }
    let mut fs = runtime::vfs::DirFs::new(host_root.clone(), Some(write_root));
    for (path, _) in session.saves.iter().filter(|(_, d)| d.is_none()) {
        runtime::vfs::Vfs::remove(&mut fs, &path.replace('/', "\\"));
    }
    let mut process = Process::new(Box::new(fs), &exe_path, &cmdline);
    process.trace = opts.trace;
    process.frame_limit = opts.frames;
    // A fixed date keeps runs reproducible (a replay uses its session's).
    process.epoch_unix = opts.epoch.or(session.epoch).unwrap_or(1_767_225_600);
    if let Some(path) = &opts.audio_out {
        process.audio = Box::new(runtime::hle::audio::WavSink { path: path.clone(), samples: Vec::new(), rate: runtime::hle::audio::RATE });
    }
    let mut eng = Engine::new(process);
    eng.trace = opts.trace;
    eng.max_api_calls = opts.max_api_calls;
    eng.recent_cap = opts.recent;
    // GMR_X87_SLOW=1: no inline x87 (every instruction through rt.fpu).
    eng.no_x87_inline = std::env::var_os("GMR_X87_SLOW").is_some();
    // GMR_NO_INLINE=1: no inlined leaf callees.
    eng.no_inline = std::env::var_os("GMR_NO_INLINE").is_some();
    // GMR_COUNT=1: translated code counts calls, x87 instructions, ... (GMR_FRAME_MS shows them).
    eng.count = std::env::var_os("GMR_COUNT").is_some();
    // GMR_COUNT=blocks: as top, by block
    eng.count_blocks = std::env::var("GMR_COUNT").is_ok_and(|v| v == "blocks");
    // GMR_API_COUNT=1: calls per API (the most called at the end).
    if std::env::var_os("GMR_API_COUNT").is_some() {
        eng.api_hist = Some(Vec::new());
    }
    // GMR_FPU_PROFILE=1: count x87 instructions by mnemonic.
    if std::env::var_os("GMR_FPU_PROFILE").is_some() {
        eng.fpu_profile = Some(Default::default());
    }
    // GMR_WATCH=hex,hex: report indirect calls to these addresses.
    if let Ok(w) = std::env::var("GMR_WATCH") {
        eng.watch = w.split(',').filter_map(|x| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok()).collect();
    }
    // GMR_PROBE=hex,hex: report every entry into these functions.
    if let Ok(w) = std::env::var("GMR_PROBE") {
        eng.probes = w.split(',').filter_map(|x| u32::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok()).collect();
    }

    let mut store = Store::new(&wt_engine, Hd { engine: eng, linker: None, t0: Instant::now(), keys: opts.keys.clone(), mouse: opts.mouse.clone(), replay: session.input, soundfont_given: false, frame_ms: std::env::var("GMR_FRAME_MS").ok().and_then(|v| v.parse().ok()), frame_mark: (0, 0, 0, 0, 0, 0, 0), net: Default::default(), samples: profile_ms.map(|_| Default::default()) });
    store.set_epoch_deadline(1);
    if profile_ms.is_some() {
        let secs = opts.seconds;
        store.epoch_deadline_callback(move |ctx| {
            if ctx.data().t0.elapsed().as_secs() >= secs {
                return Err(wasmtime::Trap::Interrupt.into());
            }
            let bt = wasmtime::WasmBacktrace::capture(&ctx);
            let names: Vec<String> = bt.frames().iter().filter_map(|f| f.func_name()).filter(|n| n.starts_with("f_")).map(str::to_owned).collect();
            let mut ctx = ctx;
            if let Some((own, total)) = ctx.data_mut().samples.as_mut() {
                *own.entry(names.first().cloned().unwrap_or_else(|| "(host)".into())).or_default() += 1;
                let mut seen = std::collections::HashSet::new();
                for n in names {
                    if seen.insert(n.clone()) {
                        *total.entry(n).or_default() += 1;
                    }
                }
            }
            Ok(wasmtime::UpdateDeadline::Continue(1))
        });
    } else {
        store.epoch_deadline_trap();
    }
    let memory = Memory::new(&mut store, MemoryType::new(MEM_PAGES as u32, Some(MEM_MAX_PAGES as u32))).map_err(|e| anyhow!("{e:?}"))?;
    let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 0, None), Ref::Func(None)).map_err(|e| anyhow!("{e:?}"))?;
    let mut globals = Vec::new();
    for (_, _, ty) in GLOBALS {
        let (vt, init) = match ty {
            wasm_encoder::ValType::I32 => (ValType::I32, Val::I32(0)),
            wasm_encoder::ValType::I64 => (ValType::I64, Val::I64(0)),
            wasm_encoder::ValType::V128 => (ValType::V128, Val::V128(0u128.into())),
            _ => unreachable!(),
        };
        globals.push(Global::new(&mut store, GlobalType::new(vt, Mutability::Var), init).map_err(|e| anyhow!("{e:?}"))?);
    }
    let cpu = Cpu { memory, table, globals: globals.clone().try_into().map_err(|_| anyhow!("globals"))? };

    let mut linker = Linker::new(&wt_engine);
    linker.define(&store, abi::CPU_MODULE, abi::MEMORY_NAME, Extern::Memory(memory)).map_err(|e| anyhow!("{e:?}"))?;
    linker.define(&store, abi::CPU_MODULE, abi::TABLE_NAME, Extern::Table(table)).map_err(|e| anyhow!("{e:?}"))?;
    for ((_, name, _), g) in GLOBALS.iter().zip(&globals) {
        linker.define(&store, abi::CPU_MODULE, name, Extern::Global(*g)).map_err(|e| anyhow!("{e:?}"))?;
    }
    let resolve = Func::wrap(&mut store, move |mut caller: Caller<'_, Hd>, addr: i32| -> i32 {
        engine::rt_resolve(&mut Wt { cx: &mut caller, cpu }, addr as u32) as i32
    });
    let trap_fn = Func::wrap(&mut store, move |mut caller: Caller<'_, Hd>, kind: i32, arg: i32| {
        engine::rt_trap(&mut Wt { cx: &mut caller, cpu }, kind, arg as u32)
    });
    let fpu_fn = Func::wrap(&mut store, move |mut caller: Caller<'_, Hd>, code: i32, a: i32, b: i32, ip: i32, fop: i32| -> i32 {
        engine::rt_fpu(&mut Wt { cx: &mut caller, cpu }, code, a as u32, b as u32, ip as u32, fop as u32)
    });
    let cpuid = Func::wrap(&mut store, move |mut caller: Caller<'_, Hd>| engine::rt_cpuid(&mut Wt { cx: &mut caller, cpu }));
    let rdtsc = Func::wrap(&mut store, move |mut caller: Caller<'_, Hd>| engine::rt_rdtsc(&mut Wt { cx: &mut caller, cpu }));
    for (f, name) in RT_FUNCS {
        let func = match f {
            abi::RtFunc::Resolve => resolve,
            abi::RtFunc::Trap => trap_fn,
            abi::RtFunc::Fpu => fpu_fn,
            abi::RtFunc::Cpuid => cpuid,
            abi::RtFunc::Rdtsc => rdtsc,
        };
        linker.define(&store, abi::RT_MODULE, name, Extern::Func(func)).map_err(|e| anyhow!("{e:?}"))?;
    }
    // The x87 fast paths every translated module imports (with fsin's and fcos's values).
    linker.func_wrap(abi::RT_MODULE, "sin", fpu::sin).map_err(|e| anyhow!("{e:?}"))?;
    linker.func_wrap(abi::RT_MODULE, "cos", fpu::cos).map_err(|e| anyhow!("{e:?}"))?;
    let x87 = Module::new(&wt_engine, recomp::x87::module(false)).map_err(|e| anyhow!("{e:?}"))?;
    let x87 = linker.instantiate(&mut store, &x87).map_err(|e| anyhow!("{e:?}"))?;
    linker.instance(&mut store, recomp::x87::MODULE, x87).map_err(|e| anyhow!("{e:?}"))?;
    store.data_mut().linker = Some(linker);

    let mut m = Wt { cx: &mut store, cpu };
    let nop = m.instantiate(&engine::nop_module(), &["nop".to_string()]).map_err(|e| anyhow!("{e}"))?;
    m.engine().nop_index = nop[0];
    let nop = m.instantiate(&engine::nop_module64(), &["nop64".to_string()]).map_err(|e| anyhow!("{e}"))?;
    m.engine().nop_index64 = nop[0];
    let started = engine::start(&mut m, &exe).map_err(|e| anyhow!("{e}"))?;
    if opts.aot {
        let t = Instant::now();
        let failed = engine::translate_all(&mut m, 512).map_err(|e| anyhow!("{e}"))?;
        eprintln!("AOT: {} functions in {:.1}s, {failed} failed", m.engine_ref().translated, t.elapsed().as_secs_f64());
    }

    let t0 = Instant::now();
    let outcome = engine::run_process(&mut m, &started);
    let elapsed = t0.elapsed().as_secs_f64();
    let final_regs = engine::read_regs(&mut m);
    let final_fpu = engine::fpu_load(&mut m);
    let (hash, chunks) = engine::state_hash(&mut m, (MEM_PAGES * 65536 - 65536) as u32);
    eprintln!("state hash: {hash:016x} ({chunks} non-zero 64K chunks)");
    if let Some(path) = &opts.dump_mem {
        std::fs::write(path, memory.data(&*m.cx))?;
    }
    for &(addr, len) in &opts.dump {
        for row in (0..len).step_by(16) {
            let mut b = vec![0u8; 16.min((len - row) as usize)];
            let _ = m.read(addr + row, &mut b);
            let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
            eprintln!("{:08x}: {}", addr + row, hex.join(" "));
        }
    }

    let e = m.engine_ref();
    let p = e.p.as_ref();
    eprintln!("---- run finished in {elapsed:.2}s ----");
    if let Some((own, total)) = &m.cx.hd_ref().samples {
        let top = |h: &std::collections::HashMap<String, u64>, n: usize| {
            let mut v: Vec<(&String, &u64)> = h.iter().collect();
            v.sort_unstable_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            v.iter().take(n).map(|(k, c)| format!("{k}:{c}")).collect::<Vec<_>>().join(" ")
        };
        eprintln!("profile, innermost ({} samples): {}", own.values().sum::<u64>(), top(own, 25));
        eprintln!("profile, on the stack: {}", top(total, 25));
    }
    match &outcome {
        Ok(()) => match e.exit {
            _ if p.is_some_and(|p| p.frames_presented >= p.frame_limit) => {
                let r = final_regs;
                eprintln!("frame limit reached");
                eprintln!(
                    "regs: eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esp={:#x} ebp={:#x} esi={:#x} edi={:#x} eip={:#x}",
                    r.eax, r.ecx, r.edx, r.ebx, r.esp, r.ebp, r.esi, r.edi, r.eip
                );
            }
            Some(code) => eprintln!("exit code {code}"),
            None => eprintln!("entry point returned (eip={:#x})", final_regs.eip),
        },
        Err(err) => {
            let r = final_regs;
            eprintln!("stopped: {err}");
            eprintln!(
                "regs: eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esp={:#x} ebp={:#x} esi={:#x} edi={:#x} eip={:#x}",
                r.eax, r.ecx, r.edx, r.ebx, r.esp, r.ebp, r.esi, r.edi, r.eip
            );
        }
    }
    eprintln!("fpu: cw={:#06x} sw={:#06x} tw={:#06x}", final_fpu.control_word(), final_fpu.status_word(), final_fpu.tag_word());
    if let Some(p) = p {
        eprintln!(
            "frames presented: {}  api calls: {}  dispatches: {}  resolves: {}  translated functions: {} in {} modules ({:.1}s)  virtual time: {}ms",
            p.frames_presented,
            p.api_calls,
            e.dispatches,
            e.resolves,
            e.translated,
            e.modules,
            e.translate_us as f64 / 1e6,
            p.now_ms()
        );
        eprintln!("x87 instructions via rt.fpu: {}", e.fpu_ops);
        if std::env::var_os("GMR_VM_STATS").is_some() {
            let mut t: std::collections::BTreeMap<&str, (u64, u64)> = Default::default();
            for r in p.vm.regions.values() {
                let e = t.entry(r.tag).or_default();
                e.0 += r.size as u64;
                e.1 += 1;
            }
            for (tag, (sz, n)) in t {
                eprintln!("vm {tag}: {} MB in {n}", sz >> 20);
            }
        }
        if let Some(h) = &e.api_hist {
            let mut v: Vec<(u64, usize)> = h.iter().enumerate().filter(|(_, n)| **n > 0).map(|(i, n)| (*n, i)).collect();
            v.sort_unstable_by(|a, b| b.cmp(a));
            let top: Vec<String> = v.iter().take(25).map(|(n, i)| format!("{} {n}", p.registry.entries[*i].clone_name())).collect();
            eprintln!("most called APIs: {}", top.join(", "));
        }
        if let Some(h) = &e.fpu_profile {
            let mut forms: std::collections::HashMap<iced_x86::Code, u64> = Default::default();
            for (&(form, _), &n) in h {
                *forms.entry(form).or_default() += n;
            }
            let mut v: Vec<_> = forms.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            let top: Vec<String> = v.iter().take(30).map(|(k, n)| format!("{k:?} {n}")).collect();
            eprintln!("  {}", top.join(", "));
            let mut sites: Vec<_> = h.iter().collect();
            sites.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = sites.iter().take(20).map(|((form, ip), n)| format!("{ip:08x} {form:?} {n}")).collect();
            eprintln!("  hottest: {}", top.join(", "));
        }
        if !e.recent.is_empty() {
            eprintln!("---- last {} dispatches (eip esp eax ebx) ----", e.recent.len());
            for r in &e.recent {
                eprintln!("  {:08x} {:08x} {:08x} {:08x}", r[0], r[1], r[2], r[3]);
            }
        }
        if !p.unimplemented.is_empty() {
            let mut u = p.unimplemented.clone();
            u.sort();
            u.dedup();
            eprintln!("unimplemented APIs: {}", u.join(", "));
        }
        let mut missing: Vec<String> = p
            .modules
            .iter()
            .flat_map(|md| md.imports.iter())
            // imports bound to a loaded DLL's exports are guest code, not HLE
            .filter(|(dll, name, _)| !p.modules.iter().any(|m| m.name.eq_ignore_ascii_case(dll) && m.exports.contains_key(name)))
            .filter(|(dll, name, _)| {
                let d = dll.to_lowercase();
                let d = if d.ends_with(".dll") { d } else { format!("{d}.dll") };
                !p.registry.data.contains_key(&(d, name.clone()))
            })
            .filter(|(dll, name, _)| p.registry.lookup(dll, name).map(|id| p.registry.func(id).is_none()).unwrap_or(true))
            .map(|(dll, name, _)| format!("{dll}!{name}"))
            .collect();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            eprintln!("---- {} statically imported APIs without an implementation ----", missing.len());
            eprintln!("{}", missing.join(" "));
        }
        // the visible top-level windows (a modal form left open stops a game that waits in it)
        let mut shown: Vec<String> = p
            .windows
            .iter()
            .filter(|(_, w)| w.style & 0x4000_0000 == 0 && w.visible)
            .map(|(h, w)| {
                // (and its controls' texts: an error form's message is in its memo)
                let texts: Vec<String> = w.children.iter().filter_map(|k| p.windows.get(k)).filter(|k| !k.title.is_empty()).map(|k| format!("{} {:?}", k.class, k.title.chars().take(400).collect::<String>())).collect();
                let texts = if texts.is_empty() { String::new() } else { format!(" [{}]", texts.join(", ")) };
                format!("{h:#x} {} {:?} {}x{}{}{texts}", w.class, w.title, w.w, w.h, if w.enabled { "" } else { " (disabled)" })
            })
            .collect();
        shown.sort();
        if !shown.is_empty() {
            eprintln!("windows: {}", shown.join(" | "));
        }
        let presents: Vec<&String> = p.log.iter().filter(|l| l.starts_with("Present #")).collect();
        for l in presents.iter().rev().take(3).rev() {
            eprintln!("{l}");
        }
        let notable: Vec<&String> = p
            .log
            .iter()
            .filter(|l| l.starts_with('[') || l.starts_with("exception") || l.starts_with("MessageBox") || l.starts_with("MCI") || l.starts_with("DirectMusic") || l.contains("not implemented") || l.contains("not registered"))
            .collect();
        if !notable.is_empty() && !opts.trace {
            eprintln!("---- notable log lines ({}) ----", notable.len());
            let mut seen = std::collections::HashSet::new();
            for l in notable.iter().filter(|l| seen.insert(l.as_str())).take(80) {
                eprintln!("{l}");
            }
        }
        if !opts.quiet && !opts.trace {
            let n = p.log.len();
            eprintln!("---- last log lines ----");
            for l in &p.log[n.saturating_sub(40)..] {
                eprintln!("{l}");
            }
        }
    }
    outcome.map_err(|e| anyhow!("{e}"))
}
