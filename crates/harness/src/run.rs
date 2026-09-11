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
    pub keys: Vec<(u64, u8, bool)>,
    /// Scripted mouse state: (after frame, x, y, buttons).
    pub mouse: Vec<(u64, i32, i32, u32)>,
    /// Write the mixed sound output to this WAV file.
    pub audio_out: Option<PathBuf>,
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
    keys: Vec<(u64, u8, bool)>,
    mouse: Vec<(u64, i32, i32, u32)>,
    /// The MIDI SoundFont was handed over (or there is none).
    soundfont_given: bool,
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
    fn xmm(&mut self, i: u32) -> u128 {
        self.cpu.globals[G::Xmm0 as usize + i as usize].get(&mut *self.cx).v128().map(|v| v.as_u128()).unwrap_or(0)
    }
    fn set_xmm(&mut self, i: u32, v: u128) {
        let _ = self.cpu.globals[G::Xmm0 as usize + i as usize].set(&mut *self.cx, Val::V128(v.into()));
    }
    fn call_index(&mut self, idx: u32) -> Result<(), String> {
        let f = match self.cpu.table.get(&mut *self.cx, idx as u64) {
            Some(Ref::Func(Some(f))) => f,
            _ => return Err(format!("table slot {idx} is empty")),
        };
        f.call(&mut *self.cx, &[], &mut []).map_err(|e| format!("{e:?}"))
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
    fn now_us(&self) -> u64 {
        self.cx.hd_ref().t0.elapsed().as_micros() as u64
    }
    fn print(&mut self, s: &str) {
        eprintln!("{s}");
    }
    fn take_input(&mut self, frame: u64) -> Vec<runtime::InputEvent> {
        let hd = self.cx.hd();
        let due: Vec<(u64, u8, bool)> = hd.keys.iter().copied().filter(|k| k.0 <= frame).collect();
        hd.keys.retain(|k| k.0 > frame);
        let mice: Vec<(u64, i32, i32, u32)> = hd.mouse.iter().copied().filter(|k| k.0 <= frame).collect();
        hd.mouse.retain(|k| k.0 > frame);
        let mut out: Vec<runtime::InputEvent> = due.into_iter().map(|(_, vk, down)| runtime::InputEvent::Key { vk, scan: 0, down }).collect();
        out.extend(mice.into_iter().map(|(_, x, y, buttons)| runtime::InputEvent::Mouse { x, y, buttons }));
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
    if let Ok(cache) = wasmtime::Cache::from_file(None) {
        cfg.cache(Some(cache));
    }
    cfg.epoch_interruption(true);
    let wt_engine = wasmtime::Engine::new(&cfg).map_err(|e| anyhow!("{e:?}"))?;
    {
        let wt_engine = wt_engine.clone();
        let secs = opts.seconds;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            wt_engine.increment_epoch();
        });
    }

    // Files the game writes go to a scratch layer, never into the imported game files.
    let slug = opts.game_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let write_root = opts.write_dir.clone().unwrap_or_else(|| PathBuf::from("target/gamedata").join(&slug));
    if !opts.keep_writes {
        let _ = std::fs::remove_dir_all(&write_root);
    }
    std::fs::create_dir_all(&write_root)?;
    let fs = runtime::vfs::DirFs::new(host_root.clone(), Some(write_root));
    let mut process = Process::new(Box::new(fs), &exe_path, &cmdline);
    process.trace = opts.trace;
    process.frame_limit = opts.frames;
    // A fixed date keeps runs reproducible.
    process.epoch_unix = 1_767_225_600;
    if let Some(path) = &opts.audio_out {
        process.audio = Box::new(runtime::hle::audio::WavSink { path: path.clone(), samples: Vec::new(), rate: runtime::hle::audio::RATE });
    }
    let mut eng = Engine::new(process);
    eng.trace = opts.trace;
    eng.max_api_calls = opts.max_api_calls;
    eng.recent_cap = opts.recent;
    // GMR_X87_SLOW=1: no inline x87 (every instruction through rt.fpu).
    eng.no_x87_inline = std::env::var_os("GMR_X87_SLOW").is_some();
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

    let mut store = Store::new(&wt_engine, Hd { engine: eng, linker: None, t0: Instant::now(), keys: opts.keys.clone(), mouse: opts.mouse.clone(), soundfont_given: false });
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();
    let memory = Memory::new(&mut store, MemoryType::new(MEM_PAGES as u32, Some(MEM_PAGES as u32))).map_err(|e| anyhow!("{e:?}"))?;
    let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 0, None), Ref::Func(None)).map_err(|e| anyhow!("{e:?}"))?;
    let mut globals = Vec::new();
    for (_, _, ty) in GLOBALS {
        let (vt, init) = match ty {
            wasm_encoder::ValType::I32 => (ValType::I32, Val::I32(0)),
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
    // The x87 fast paths every translated module imports.
    let x87 = Module::new(&wt_engine, recomp::x87::module(false)).map_err(|e| anyhow!("{e:?}"))?;
    let x87 = linker.instantiate(&mut store, &x87).map_err(|e| anyhow!("{e:?}"))?;
    linker.instance(&mut store, recomp::x87::MODULE, x87).map_err(|e| anyhow!("{e:?}"))?;
    store.data_mut().linker = Some(linker);

    let mut m = Wt { cx: &mut store, cpu };
    let nop = m.instantiate(&engine::nop_module(), &["nop".to_string()]).map_err(|e| anyhow!("{e}"))?;
    m.engine().nop_index = nop[0];
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
    match &outcome {
        Ok(()) => match e.exit {
            _ if p.is_some_and(|p| p.frames_presented >= p.frame_limit) => eprintln!("frame limit reached"),
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
            "frames presented: {}  api calls: {}  dispatches: {}  resolves: {}  translated functions: {} ({:.1}s)  virtual time: {}ms",
            p.frames_presented,
            p.api_calls,
            e.dispatches,
            e.resolves,
            e.translated,
            e.translate_us as f64 / 1e6,
            p.now_ms()
        );
        eprintln!("x87 instructions via rt.fpu: {}", e.fpu_ops);
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
