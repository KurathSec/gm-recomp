//! `harness run`: executes a game under wasmtime with the HLE runtime. Guest code
//! is translated on demand (or ahead of time with `--aot`) into wasm modules that
//! share one memory, one function table and one set of CPU-state globals.
//!
//! Control transfer contract (see recomp::abi): every translated function returns
//! when the guest executes `ret`, leaving the continuation in the `eip` global.
//! The dispatcher loop resolves `eip` to a function (translating if needed) and
//! calls it; an HLE stub address runs the API inside `resolve` and hands back a
//! no-op function so the caller's continuation check passes.

use anyhow::{Context, Result, anyhow};
use recomp::abi::{self, GLOBALS, Global as G, RT_FUNCS, trap};
use recomp::cfg::FuncInfo;
use runtime::hle::{ApiError, ApiId, Ctx};
use runtime::{Guest, Host, Process, RET_TO_HOST, Regs, StubResolver};
use runtime::mem::Fault;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;
use wasmtime::{
    AsContext, AsContextMut, Caller, Config, Engine, Extern, Func, Global, GlobalType, Linker, Memory, MemoryType, Module,
    Mutability, Ref, RefType, Store, Table, TableType, Val, ValType,
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
}

#[derive(Clone, Copy)]
struct Cpu {
    memory: Memory,
    table: Table,
    globals: [Global; 26],
}

struct ModInfo {
    name: String,
    base: u32,
    size: u32,
    functions: HashSet<u32>,
    jump_tables: HashMap<u32, Vec<u32>>,
}

struct State {
    p: Option<Process>,
    fpu: fpu::Fpu,
    /// Guest entry address -> table index.
    funcs: HashMap<u32, u32>,
    nop_index: u32,
    mods: Vec<ModInfo>,
    linker: Option<Linker<State>>,
    exit: Option<u32>,
    /// A trap handler rewrote the registers; the dispatcher continues at `eip`.
    resume: bool,
    pending_err: Option<String>,
    depth: u32,
    translated: usize,
    translate_time: f64,
    trace: bool,
    max_api_calls: u64,
    tsc: u64,
    bad_functions: HashSet<u32>,
    /// Ring buffer of the most recent dispatcher targets (eip, esp, eax, ebx).
    recent: std::collections::VecDeque<[u32; 4]>,
    recent_cap: usize,
    dispatches: u64,
}

// ---- guest memory as seen by the HLE layer ------------------------------------------

struct View<'a, S: AsContextMut<Data = State>> {
    cx: &'a mut S,
    cpu: Cpu,
}

impl<S: AsContextMut<Data = State>> Guest for View<'_, S> {
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

impl<S: AsContextMut<Data = State>> Host for View<'_, S> {
    fn call_guest(&mut self, p: &mut Process, regs: &mut Regs, addr: u32, args: &[u32]) -> Result<u32, String> {
        let depth = {
            let mut b = self.cx.as_context_mut();
            let st = b.data_mut();
            st.depth += 1;
            st.depth
        };
        let marker = RET_TO_HOST + 4 * depth;
        // Build the callee's frame below everything the API has on the stack.
        let mut esp = (regs.esp - 0x80) & !0xF;
        for a in args.iter().rev() {
            esp -= 4;
            self.write_u32(esp, *a).map_err(|e| e.to_string())?;
        }
        esp -= 4;
        self.write_u32(esp, marker).map_err(|e| e.to_string())?;
        let mut callee = *regs;
        callee.esp = esp;
        callee.eip = addr;
        write_regs(self.cx, &self.cpu, &callee);
        // Lend the process to the nested dispatcher.
        let owned = std::mem::replace(p, Process::empty());
        self.cx.as_context_mut().data_mut().p = Some(owned);
        let r = run_until(self.cx, &self.cpu, addr, marker);
        {
            let mut b = self.cx.as_context_mut();
            let st = b.data_mut();
            st.depth -= 1;
            if let Some(back) = st.p.take() {
                *p = back;
            }
        }
        let after = read_regs(self.cx, &self.cpu);
        // Callee-saved registers come back as the callee left them; the API's own
        // stack pointer is restored since the callee popped its frame.
        let eax = after.eax;
        regs.eax = after.eax;
        regs.ecx = after.ecx;
        regs.edx = after.edx;
        regs.ebx = after.ebx;
        regs.esi = after.esi;
        regs.edi = after.edi;
        regs.ebp = after.ebp;
        match r {
            Ok(()) => Ok(eax),
            Err(e) => Err(format!("{e:#}")),
        }
    }
    fn request_exit(&mut self, code: u32) {
        self.cx.as_context_mut().data_mut().exit = Some(code);
    }
    fn fpu_push(&mut self, v: f64) {
        let mut b = self.cx.as_context_mut();
        let st = b.data_mut();
        let mut m = Scratch(v.to_bits().to_le_bytes().to_vec());
        st.fpu.exec(iced_x86::Mnemonic::Fld, 0, (iced_x86::MemorySize::Float64 as u32) << 8, 0, 0, &mut m);
    }
    fn fpu_pop(&mut self) -> f64 {
        let mut b = self.cx.as_context_mut();
        let st = b.data_mut();
        let mut m = Scratch(vec![0; 8]);
        st.fpu.exec(iced_x86::Mnemonic::Fstp, 0, (iced_x86::MemorySize::Float64 as u32) << 8, 0, 0, &mut m);
        f64::from_bits(u64::from_le_bytes(m.0[..8].try_into().unwrap()))
    }
}

/// An 8-byte memory operand at address 0 for host-initiated x87 loads and stores.
struct Scratch(Vec<u8>);
impl fpu::Mem for Scratch {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        let a = addr as usize;
        buf.copy_from_slice(&self.0[a..a + buf.len()]);
    }
    fn write(&mut self, addr: u32, data: &[u8]) {
        let a = addr as usize;
        self.0[a..a + data.len()].copy_from_slice(data);
    }
}

// ---- register access -------------------------------------------------------------------

fn get_u32<S: AsContextMut>(cx: &mut S, g: Global) -> u32 {
    g.get(&mut *cx).i32().unwrap_or(0) as u32
}

fn set_u32<S: AsContextMut>(cx: &mut S, g: Global, v: u32) {
    let _ = g.set(&mut *cx, Val::I32(v as i32));
}

fn read_regs<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu) -> Regs {
    Regs {
        eax: get_u32(cx, cpu.globals[G::Eax as usize]),
        ecx: get_u32(cx, cpu.globals[G::Ecx as usize]),
        edx: get_u32(cx, cpu.globals[G::Edx as usize]),
        ebx: get_u32(cx, cpu.globals[G::Ebx as usize]),
        esp: get_u32(cx, cpu.globals[G::Esp as usize]),
        ebp: get_u32(cx, cpu.globals[G::Ebp as usize]),
        esi: get_u32(cx, cpu.globals[G::Esi as usize]),
        edi: get_u32(cx, cpu.globals[G::Edi as usize]),
        eip: get_u32(cx, cpu.globals[G::Eip as usize]),
        fs_base: get_u32(cx, cpu.globals[G::FsBase as usize]),
    }
}

fn write_regs<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu, r: &Regs) {
    set_u32(cx, cpu.globals[G::Eax as usize], r.eax);
    set_u32(cx, cpu.globals[G::Ecx as usize], r.ecx);
    set_u32(cx, cpu.globals[G::Edx as usize], r.edx);
    set_u32(cx, cpu.globals[G::Ebx as usize], r.ebx);
    set_u32(cx, cpu.globals[G::Esp as usize], r.esp);
    set_u32(cx, cpu.globals[G::Ebp as usize], r.ebp);
    set_u32(cx, cpu.globals[G::Esi as usize], r.esi);
    set_u32(cx, cpu.globals[G::Edi as usize], r.edi);
    set_u32(cx, cpu.globals[G::Eip as usize], r.eip);
    set_u32(cx, cpu.globals[G::FsBase as usize], r.fs_base);
}

fn mem_u32<S: AsContext>(cx: &S, cpu: &Cpu, addr: u32) -> u32 {
    let d = cpu.memory.data(cx);
    match d.get(addr as usize..addr as usize + 4) {
        Some(b) => u32::from_le_bytes(b.try_into().unwrap()),
        None => 0,
    }
}

// ---- API dispatch --------------------------------------------------------------------------

/// Runs `f` with the process checked out of the store and a `Ctx` built on it.
fn with_ctx<S: AsContextMut<Data = State>, R>(cx: &mut S, cpu: &Cpu, regs: &mut Regs, id: ApiId, f: impl FnOnce(&mut Ctx) -> R) -> Result<R> {
    let mut p = cx.as_context_mut().data_mut().p.take().ok_or_else(|| anyhow!("process not available (nested host call without lending)"))?;
    let r = {
        let mut view = View { cx: &mut *cx, cpu: *cpu };
        let mut ctx = Ctx { host: &mut view, regs, p: &mut p, id };
        f(&mut ctx)
    };
    cx.as_context_mut().data_mut().p = Some(p);
    Ok(r)
}

fn format_args<S: AsContext>(cx: &S, cpu: &Cpu, esp: u32, n: u32) -> String {
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!("{:#x}", mem_u32(cx, cpu, esp + 4 + 4 * i)));
    }
    s
}

fn call_api<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu, id: ApiId) -> Result<()> {
    let mut regs = read_regs(cx, cpu);
    let (trace, depth, max_calls) = {
        let st = cx.as_context().data();
        (st.trace, st.depth, st.max_api_calls)
    };
    let (name, func, nargs, calls) = {
        let mut b = cx.as_context_mut();
        let st = b.data_mut();
        let p = st.p.as_mut().ok_or_else(|| anyhow!("process not available"))?;
        p.api_calls += 1;
        if !p.registry.idle_ok.contains(&id.0) {
            p.idle_streak = 0;
        }
        let e = &p.registry.entries[id.0 as usize];
        (format!("{}!{}", e.dll, e.name), e.func, e.nargs, p.api_calls)
    };
    if calls > max_calls {
        return Err(anyhow!("API call budget exhausted ({max_calls})"));
    }
    let ra = mem_u32(cx, cpu, regs.esp);
    if trace {
        let args = format_args(cx, cpu, regs.esp, if nargs > 0 { nargs.min(8) } else { 4 });
        eprintln!("{:w$}{name}({args}) from {ra:#x}", "", w = (depth * 2) as usize);
    }
    let Some(f) = func else {
        with_ctx(cx, cpu, &mut regs, id, |c| c.p.unimplemented.push(name.clone()))?;
        return Err(anyhow!("unimplemented API {name} called from {ra:#x}"));
    };
    let result = with_ctx(cx, cpu, &mut regs, id, |c| f(c))?;
    match result {
        Ok(ret) => {
            if ret.resume {
                if trace {
                    eprintln!("{:w$}  -> resume at {:#x}", "", regs.eip, w = (depth * 2) as usize);
                }
            } else {
                regs.eax = ret.eax;
                if let Some(edx) = ret.edx {
                    regs.edx = edx;
                }
                regs.esp += 4 + ret.pop;
                regs.eip = ra;
                if trace {
                    eprintln!("{:w$}  = {:#x}", "", ret.eax, w = (depth * 2) as usize);
                }
            }
            write_regs(cx, cpu, &regs);
            Ok(())
        }
        Err(ApiError(msg)) => Err(anyhow!("{name}: {msg}")),
    }
}

/// Raises a hardware-style exception through the guest's SEH chain from a trap.
fn raise_hw_exception<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu, code: u32, ip: u32) -> Result<()> {
    let mut regs = read_regs(cx, cpu);
    regs.eip = ip;
    let id = {
        let st = cx.as_context().data();
        st.p.as_ref().and_then(|p| p.registry.lookup("kernel32.dll", "RaiseException")).ok_or_else(|| anyhow!("RaiseException not registered"))?
    };
    let ctx_regs = regs;
    let r = with_ctx(cx, cpu, &mut regs, id, |c| runtime::hle::kernel32::dispatch_exception(c, code, 0, ip, &[], &ctx_regs))?;
    match r {
        Ok(ret) if ret.resume => {
            write_regs(cx, cpu, &regs);
            cx.as_context_mut().data_mut().resume = true;
            Ok(())
        }
        Ok(_) => Err(anyhow!("exception {code:#x} at {ip:#x}: handler returned without a context")),
        Err(ApiError(m)) => Err(anyhow!("{m}")),
    }
}

// ---- translation -----------------------------------------------------------------------------

struct SliceMem<'a>(&'a [u8]);

impl recomp::Memory for SliceMem<'_> {
    fn read(&self, va: u32, buf: &mut [u8]) -> bool {
        match self.0.get(va as usize..va as usize + buf.len()) {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
}

/// Registers newly loaded modules (discovery runs once per image).
fn sync_modules<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu) {
    let new: Vec<(String, u32, u32, Vec<u8>)> = {
        let st = cx.as_context().data();
        let Some(p) = st.p.as_ref() else { return };
        p.modules
            .iter()
            .filter(|m| !st.mods.iter().any(|k| k.base == m.base))
            .map(|m| (m.name.clone(), m.base, m.size, m.file.clone()))
            .collect()
    };
    for (name, base, size, file) in new {
        let t = Instant::now();
        let mut functions = HashSet::new();
        let mut jump_tables = HashMap::new();
        match pe::Pe::parse(&file) {
            Ok(pe) => {
                let d = x86::discover(&pe);
                for f in &d.function_list {
                    functions.insert(base + f.rva);
                }
                for jt in &d.jump_tables {
                    let mut targets = Vec::new();
                    for i in 0..jt.entries {
                        targets.push(mem_u32(&*cx, cpu, base + jt.table_rva + 4 * i));
                    }
                    jump_tables.insert(base + jt.site_rva, targets);
                }
                if !cx.as_context().data().trace {
                    eprintln!("[{name}] discovery: {} functions, {} jump tables ({:.1}s)", functions.len(), jump_tables.len(), t.elapsed().as_secs_f64());
                }
            }
            Err(e) => eprintln!("[{name}] discovery failed: {e}"),
        }
        cx.as_context_mut().data_mut().mods.push(ModInfo { name, base, size, functions, jump_tables });
    }
}

/// Translates the functions starting at `entries` into one wasm module and registers
/// them in the table. Returns the entries that failed.
fn translate_batch<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu, entries: &[u32]) -> Result<Vec<u32>> {
    let t = Instant::now();
    let output = {
        let st = cx.as_context().data();
        let data = cpu.memory.data(&*cx);
        let mem = SliceMem(data);
        let mut tr = recomp::Translator::new(&mem, recomp::Options { shared_memory: false });
        let mut failed = Vec::new();
        for &entry in entries {
            let m = st.mods.iter().find(|m| entry >= m.base && entry < m.base + m.size);
            let empty_jt = HashMap::new();
            let jt = m.map(|m| &m.jump_tables).unwrap_or(&empty_jt);
            let is_fn = |a: u32| st.funcs.contains_key(&a) || m.map(|m| m.functions.contains(&a)).unwrap_or(false);
            let info = FuncInfo { jump_tables: jt, is_function: &is_fn };
            if let Err(e) = tr.add_function(entry, &info) {
                eprintln!("translate {entry:#x}: {e}");
                failed.push(entry);
            }
        }
        let out = tr.finish();
        (out, failed)
    };
    let (out, mut failed) = output;
    if out.functions.is_empty() {
        return Ok(failed);
    }
    let engine = cx.as_context().engine().clone();
    let module = match Module::new(&engine, &out.wasm) {
        Ok(m) => m,
        Err(e) => {
            if entries.len() == 1 {
                let _ = std::fs::write("target/failed.wasm", &out.wasm);
                return Err(anyhow!("wasm for function {:#x} rejected: {e} (written to target/failed.wasm)", entries[0]));
            }
            // Bisect to isolate the offending function.
            let (a, b) = entries.split_at(entries.len() / 2);
            failed.extend(translate_batch(cx, cpu, a)?);
            failed.extend(translate_batch(cx, cpu, b)?);
            return Ok(failed);
        }
    };
    let linker = cx.as_context().data().linker.clone().ok_or_else(|| anyhow!("linker missing"))?;
    let instance = linker.instantiate(&mut *cx, &module).map_err(|e| anyhow!("instantiate: {e:?}"))?;
    for (entry, name) in &out.functions {
        let f = instance.get_func(&mut *cx, name).ok_or_else(|| anyhow!("missing export {name}"))?;
        let idx = cpu.table.grow(&mut *cx, 1, Ref::Func(Some(f))).map_err(|e| anyhow!("table grow: {e:?}"))?;
        cx.as_context_mut().data_mut().funcs.insert(*entry, idx as u32);
    }
    let mut b = cx.as_context_mut();
    let st = b.data_mut();
    st.translated += out.functions.len();
    st.translate_time += t.elapsed().as_secs_f64();
    if st.trace {
        for (addr, text) in &out.unimplemented {
            eprintln!("unimplemented instruction at {addr:#x}: {text}");
        }
    }
    Ok(failed)
}

fn resolve_addr<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu, addr: u32) -> Result<u32> {
    if let Some(&i) = cx.as_context().data().funcs.get(&addr) {
        return Ok(i);
    }
    if let Some(id) = Process::stub_id(addr) {
        call_api(cx, cpu, id)?;
        return Ok(cx.as_context().data().nop_index);
    }
    if (RET_TO_HOST..RET_TO_HOST + 0x1000).contains(&addr) {
        return Err(anyhow!("guest returned to a host frame marker {addr:#x} that is not the innermost one"));
    }
    if cx.as_context().data().bad_functions.contains(&addr) {
        return Err(anyhow!("function {addr:#x} cannot be translated"));
    }
    sync_modules(cx, cpu);
    let failed = translate_batch(cx, cpu, &[addr])?;
    if !failed.is_empty() {
        cx.as_context_mut().data_mut().bad_functions.insert(addr);
        return Err(anyhow!("cannot translate function at {addr:#x}"));
    }
    cx.as_context().data().funcs.get(&addr).copied().ok_or_else(|| anyhow!("translation of {addr:#x} produced no function"))
}

// ---- dispatcher -----------------------------------------------------------------------------------

/// Runs guest code starting at `addr` until it returns to `marker`.
fn run_until<S: AsContextMut<Data = State>>(cx: &mut S, cpu: &Cpu, addr: u32, marker: u32) -> Result<()> {
    let mut eip = addr;
    loop {
        {
            let cap = cx.as_context().data().recent_cap;
            let rec = if cap > 0 {
                let esp = get_u32(cx, cpu.globals[G::Esp as usize]);
                let eax = get_u32(cx, cpu.globals[G::Eax as usize]);
                let ebx = get_u32(cx, cpu.globals[G::Ebx as usize]);
                Some([eip, esp, eax, ebx])
            } else {
                None
            };
            let mut b = cx.as_context_mut();
            let st = b.data_mut();
            st.dispatches += 1;
            if let Some(r) = rec {
                if st.recent.len() == cap {
                    st.recent.pop_front();
                }
                st.recent.push_back(r);
            }
        }
        let idx = resolve_addr(cx, cpu, eip)?;
        let f = match cpu.table.get(&mut *cx, idx as u64) {
            Some(Ref::Func(Some(f))) => f,
            _ => return Err(anyhow!("table slot {idx} is empty")),
        };
        let r = f.call(&mut *cx, &[], &mut []);
        if let Err(e) = r {
            let mut b = cx.as_context_mut();
            let st = b.data_mut();
            if st.resume {
                st.resume = false;
            } else if st.exit.is_some() {
                return Ok(());
            } else {
                let msg = st.pending_err.take().unwrap_or_else(|| format!("{e:?}"));
                return Err(anyhow!("{msg}"));
            }
        }
        {
            let st = cx.as_context().data();
            if st.exit.is_some() {
                return Ok(());
            }
            if let Some(p) = st.p.as_ref() {
                if p.frames_presented >= p.frame_limit && marker == RET_TO_HOST {
                    return Ok(());
                }
            }
        }
        eip = get_u32(cx, cpu.globals[G::Eip as usize]);
        if eip == marker {
            return Ok(());
        }
    }
}

// ---- setup ------------------------------------------------------------------------------------------

struct MemView<'a>(&'a mut [u8]);
impl fpu::Mem for MemView<'_> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        if let Some(src) = self.0.get(addr as usize..addr as usize + buf.len()) {
            buf.copy_from_slice(src);
        }
    }
    fn write(&mut self, addr: u32, data: &[u8]) {
        if let Some(dst) = self.0.get_mut(addr as usize..addr as usize + data.len()) {
            dst.copy_from_slice(data);
        }
    }
}

fn nop_module() -> Vec<u8> {
    use wasm_encoder::{CodeSection, ExportKind, ExportSection, Function, FunctionSection, Module, TypeSection};
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    let mut exports = ExportSection::new();
    exports.export("nop", ExportKind::Func, 0);
    let mut code = CodeSection::new();
    let mut f = Function::new([]);
    f.instruction(&wasm_encoder::Instruction::End);
    code.function(&f);
    let mut m = Module::new();
    m.section(&types);
    m.section(&funcs);
    m.section(&exports);
    m.section(&code);
    m.finish()
}

fn cpuid_regs(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    let _ = sub;
    match leaf {
        0 => (1, 0x756E_6547, 0x6C65_746E, 0x4965_6E69),
        1 => (0x0001_06A5, 0x0001_0800, 0, 0x0780_8111),
        0x8000_0000 => (0x8000_0000, 0, 0, 0),
        _ => (0, 0, 0, 0),
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
    let engine = Engine::new(&cfg).map_err(|e| anyhow!("{e:?}"))?;
    {
        let engine = engine.clone();
        let secs = opts.seconds;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            engine.increment_epoch();
        });
    }

    let mut process = Process::new(host_root.clone(), &exe_path, &cmdline);
    // Files the game writes go to a scratch layer, never into the imported game files.
    let slug = opts.game_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let write_root = PathBuf::from("target/gamedata").join(&slug);
    if !opts.keep_writes {
        let _ = std::fs::remove_dir_all(&write_root);
    }
    std::fs::create_dir_all(&write_root)?;
    process.write_root = Some(write_root);
    process.trace = opts.trace;
    process.frame_limit = opts.frames;
    let state = State {
        p: Some(process),
        fpu: fpu::Fpu::new(),
        funcs: HashMap::new(),
        nop_index: 0,
        mods: Vec::new(),
        linker: None,
        exit: None,
        resume: false,
        pending_err: None,
        depth: 0,
        translated: 0,
        translate_time: 0.0,
        trace: opts.trace,
        max_api_calls: opts.max_api_calls,
        tsc: 0,
        bad_functions: HashSet::new(),
        recent: std::collections::VecDeque::new(),
        recent_cap: opts.recent,
        dispatches: 0,
    };
    let mut store = Store::new(&engine, state);
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

    let mut linker = Linker::new(&engine);
    linker.define(&store, abi::CPU_MODULE, abi::MEMORY_NAME, Extern::Memory(memory)).map_err(|e| anyhow!("{e:?}"))?;
    linker.define(&store, abi::CPU_MODULE, abi::TABLE_NAME, Extern::Table(table)).map_err(|e| anyhow!("{e:?}"))?;
    for ((_, name, _), g) in GLOBALS.iter().zip(&globals) {
        linker.define(&store, abi::CPU_MODULE, name, Extern::Global(*g)).map_err(|e| anyhow!("{e:?}"))?;
    }
    let cpu_r = cpu;
    let resolve = Func::wrap(&mut store, move |mut caller: Caller<'_, State>, addr: i32| -> wasmtime::Result<i32> {
        match resolve_addr(&mut caller, &cpu_r, addr as u32) {
            Ok(i) => Ok(i as i32),
            Err(e) => {
                let st = caller.data_mut();
                if st.pending_err.is_none() {
                    st.pending_err = Some(format!("{e:#}"));
                }
                Err(wasmtime::Error::msg("guest stopped"))
            }
        }
    });
    let cpu_t = cpu;
    let trap_fn = Func::wrap(&mut store, move |mut caller: Caller<'_, State>, kind: i32, arg: i32| -> wasmtime::Result<()> {
        let arg = arg as u32;
        let eip = get_u32(&mut caller, cpu_t.globals[G::Eip as usize]);
        let r: Result<()> = match kind {
            trap::INT3 => {
                eprintln!("int3 at {arg:#x} (ignored)");
                Ok(())
            }
            trap::INT => Err(anyhow!("int {arg:#x} at {eip:#x}")),
            trap::DIVIDE_ERROR => raise_hw_exception(&mut caller, &cpu_t, 0xC000_0094, arg).map(|_| ()),
            trap::UD2 => raise_hw_exception(&mut caller, &cpu_t, 0xC000_001D, arg),
            trap::HLT => Err(anyhow!("hlt at {arg:#x}")),
            trap::NO_TARGET => Err(anyhow!("branch to an unresolvable target at {arg:#x}")),
            trap::UNIMPLEMENTED => Err(anyhow!("unimplemented instruction at {arg:#x}")),
            trap::FPU_EXCEPTION => Err(anyhow!("x87 exception at {arg:#x}")),
            _ => Err(anyhow!("trap {kind} at {arg:#x}")),
        };
        match r {
            Ok(()) => {
                if caller.data().resume {
                    Err(wasmtime::Error::msg("resume"))
                } else {
                    Ok(())
                }
            }
            Err(e) => {
                let st = caller.data_mut();
                if st.pending_err.is_none() {
                    st.pending_err = Some(format!("{e:#}"));
                }
                Err(wasmtime::Error::msg("guest stopped"))
            }
        }
    });
    let cpu_f = cpu;
    let fpu_fn = Func::wrap(&mut store, move |mut caller: Caller<'_, State>, code: i32, a: i32, b: i32, ip: i32, fop: i32| -> wasmtime::Result<i32> {
        let mn = iced_x86::Code::try_from(code as usize).map(|c| c.mnemonic()).unwrap_or(iced_x86::Mnemonic::INVALID);
        if caller.data().fpu.fault_pending(mn) {
            let sw = caller.data().fpu.status_word();
            let cw = caller.data().fpu.control_word();
            let pending = sw & !cw & 0x3F;
            let status = if pending & 1 != 0 {
                0xC000_0090
            } else if pending & 4 != 0 {
                0xC000_008E
            } else if pending & 8 != 0 {
                0xC000_0091
            } else if pending & 0x10 != 0 {
                0xC000_0093
            } else if pending & 2 != 0 {
                0xC000_008D
            } else {
                0xC000_008F
            };
            // The OS reports the fault and the Delphi RTL clears the FPU before continuing.
            caller.data_mut().fpu.set_status_word(sw & !0x80BF);
            return match raise_hw_exception(&mut caller, &cpu_f, status, ip as u32) {
                Ok(()) => Err(wasmtime::Error::msg("resume")),
                Err(e) => {
                    let st = caller.data_mut();
                    if st.pending_err.is_none() {
                        st.pending_err = Some(format!("{e:#}"));
                    }
                    Err(wasmtime::Error::msg("guest stopped"))
                }
            };
        }
        let mut f = std::mem::take(&mut caller.data_mut().fpu);
        let r = f.exec(mn, a as u32, b as u32, ip as u32, fop as u16, &mut MemView(cpu_f.memory.data_mut(&mut caller)));
        caller.data_mut().fpu = f;
        Ok(r)
    });
    let cpu_c = cpu;
    let cpuid = Func::wrap(&mut store, move |mut caller: Caller<'_, State>| {
        let leaf = get_u32(&mut caller, cpu_c.globals[G::Eax as usize]);
        let sub = get_u32(&mut caller, cpu_c.globals[G::Ecx as usize]);
        let (a, b, c, d) = cpuid_regs(leaf, sub);
        set_u32(&mut caller, cpu_c.globals[G::Eax as usize], a);
        set_u32(&mut caller, cpu_c.globals[G::Ebx as usize], b);
        set_u32(&mut caller, cpu_c.globals[G::Ecx as usize], c);
        set_u32(&mut caller, cpu_c.globals[G::Edx as usize], d);
    });
    let cpu_d = cpu;
    let rdtsc = Func::wrap(&mut store, move |mut caller: Caller<'_, State>| {
        let us = caller.data().p.as_ref().map(|p| p.clock_us).unwrap_or(0);
        let st = caller.data_mut();
        st.tsc = st.tsc.max(us * 3000) + 1000;
        let v = st.tsc;
        set_u32(&mut caller, cpu_d.globals[G::Eax as usize], v as u32);
        set_u32(&mut caller, cpu_d.globals[G::Edx as usize], (v >> 32) as u32);
    });
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
    // Table slot 0: the no-op function returned by `resolve` after an API call.
    let nop = Module::new(&engine, &nop_module()).map_err(|e| anyhow!("{e:?}"))?;
    let nop_inst = linker.instantiate(&mut store, &nop).map_err(|e| anyhow!("{e:?}"))?;
    let nop_fn = nop_inst.get_func(&mut store, "nop").unwrap();
    let nop_index = table.grow(&mut store, 1, Ref::Func(Some(nop_fn))).map_err(|e| anyhow!("{e:?}"))?;
    store.data_mut().nop_index = nop_index as u32;
    store.data_mut().linker = Some(linker);

    // Load the executable and the game DLLs it imports statically.
    let mut loaded = HashSet::new();
    let (entry, image_base, attach_list) = load_images(&mut store, &cpu, &host_root, &exe, &mut loaded)?;
    let (teb, stack_top) = {
        let mut view = View { cx: &mut store, cpu };
        let mut p = view.cx.data_mut().p.take().unwrap();
        let r = p.setup_main_thread(&mut view, image_base).map_err(|e| anyhow!("{e}"))?;
        p.setup_static_tls(&mut view).map_err(|e| anyhow!("{e}"))?;
        view.cx.data_mut().p = Some(p);
        r
    };
    set_u32(&mut store, cpu.globals[G::FsBase as usize], teb);
    set_u32(&mut store, cpu.globals[G::Mxcsr as usize], 0x1F80);
    let esp = stack_top - 0x100;
    let regs = Regs { eax: 0, ecx: 0, edx: 0, ebx: 0, esp, ebp: 0, esi: 0, edi: 0, eip: entry, fs_base: teb };
    write_regs(&mut store, &cpu, &regs);

    if opts.aot {
        sync_modules(&mut store, &cpu);
        let entries: Vec<u32> = store.data().mods.iter().flat_map(|m| m.functions.iter().copied()).collect();
        eprintln!("AOT: translating {} functions", entries.len());
        let t = Instant::now();
        let mut failed = Vec::new();
        for chunk in entries.chunks(512) {
            failed.extend(translate_batch(&mut store, &cpu, chunk)?);
        }
        eprintln!("AOT: done in {:.1}s, {} failed", t.elapsed().as_secs_f64(), failed.len());
    }

    // DllMain of statically imported game DLLs, then the entry point.
    let t0 = Instant::now();
    let mut outcome: Result<()> = Ok(());
    for (base, dll_entry, name) in attach_list {
        let marker = RET_TO_HOST;
        let mut esp = get_u32(&mut store, cpu.globals[G::Esp as usize]);
        for a in [0u32, 1, base].iter() {
            esp -= 4;
            let d = memory.data_mut(&mut store);
            d[esp as usize..esp as usize + 4].copy_from_slice(&a.to_le_bytes());
        }
        esp -= 4;
        memory.data_mut(&mut store)[esp as usize..esp as usize + 4].copy_from_slice(&marker.to_le_bytes());
        set_u32(&mut store, cpu.globals[G::Esp as usize], esp);
        eprintln!("DllMain({name}) at {dll_entry:#x}");
        outcome = run_until(&mut store, &cpu, dll_entry, marker);
        if outcome.is_err() {
            break;
        }
        let esp = get_u32(&mut store, cpu.globals[G::Esp as usize]);
        set_u32(&mut store, cpu.globals[G::Esp as usize], esp + 12);
    }
    if outcome.is_ok() {
        let esp = get_u32(&mut store, cpu.globals[G::Esp as usize]) - 4;
        memory.data_mut(&mut store)[esp as usize..esp as usize + 4].copy_from_slice(&RET_TO_HOST.to_le_bytes());
        set_u32(&mut store, cpu.globals[G::Esp as usize], esp);
        outcome = run_until(&mut store, &cpu, entry, RET_TO_HOST);
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let final_regs = read_regs(&mut store, &cpu);
    for &(addr, len) in &opts.dump {
        let data = memory.data(&store);
        for row in (0..len).step_by(16) {
            let a = (addr + row) as usize;
            let bytes: Vec<String> = data.get(a..a + 16.min((len - row) as usize)).unwrap_or(&[]).iter().map(|b| format!("{b:02x}")).collect();
            eprintln!("{:08x}: {}", addr + row, bytes.join(" "));
        }
    }

    if let Err(e) = &outcome {
        if format!("{e:#}").ends_with(runtime::hle::FRAME_LIMIT) {
            outcome = Ok(());
        }
    }
    let st = store.data();
    let p = st.p.as_ref();
    eprintln!("---- run finished in {elapsed:.2}s ----");
    match &outcome {
        Ok(()) => match st.exit {
            _ if p.is_some_and(|p| p.frames_presented >= p.frame_limit) => eprintln!("frame limit reached"),
            Some(code) => eprintln!("exit code {code}"),
            None => eprintln!("entry point returned (eip={:#x})", final_regs.eip),
        },
        Err(e) => {
            let regs = final_regs;
            eprintln!("stopped: {e:#}");
            eprintln!(
                "regs: eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esp={:#x} ebp={:#x} esi={:#x} edi={:#x} eip={:#x}",
                regs.eax, regs.ecx, regs.edx, regs.ebx, regs.esp, regs.ebp, regs.esi, regs.edi, regs.eip
            );
        }
    }
    eprintln!("fpu: cw={:#06x} sw={:#06x} tw={:#06x}", st.fpu.control_word(), st.fpu.status_word(), st.fpu.tag_word());
    if let Some(p) = p {
        eprintln!("frames presented: {}  api calls: {}  dispatches: {}  translated functions: {} ({:.1}s)  virtual time: {}ms", p.frames_presented, p.api_calls, st.dispatches, st.translated, st.translate_time, p.now_ms());
        if !st.recent.is_empty() {
            eprintln!("---- last {} dispatches (eip esp eax ebx) ----", st.recent.len());
            for r in &st.recent {
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
            .flat_map(|m| m.imports.iter())
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
        let notable: Vec<&String> = p.log.iter().filter(|l| l.starts_with('[') || l.starts_with("exception") || l.starts_with("MessageBox") || l.contains("not implemented") || l.contains("not registered")).collect();
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
    outcome
}

/// Maps the exe and the game DLLs it imports (dependencies first). Returns the exe
/// entry, its base and the DllMain calls to make before running the entry.
fn load_images(store: &mut Store<State>, cpu: &Cpu, host_root: &Path, exe: &str, loaded: &mut HashSet<String>) -> Result<(u32, u32, Vec<(u32, u32, String)>)> {
    let mut attach = Vec::new();
    let mut order = Vec::new();
    collect_order(host_root, exe, loaded, &mut order)?;
    let mut exe_entry = 0;
    let mut exe_base = 0;
    for name in order {
        let path = host_root.join(&name);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let guest_path = format!("C:\\game\\{name}");
        let is_exe = name.eq_ignore_ascii_case(exe);
        let mut view = View { cx: &mut *store, cpu: *cpu };
        let mut p = view.cx.data_mut().p.take().unwrap();
        let m = {
            let req = runtime::LoadRequest { name: &name, path: &guest_path, bytes };
            let mut resolver = StubResolver { registry: &mut p.registry, modules: &p.modules };
            runtime::loader::load(req, &mut p.vm, &mut view, &mut resolver).map_err(|e| anyhow!("{e}"))?
        };
        eprintln!("loaded {name} at {:#x} ({} KB, entry {:#x}, {} imports)", m.base, m.size / 1024, m.entry, m.imports.len());
        if is_exe {
            exe_entry = m.entry;
            exe_base = m.base;
        } else if m.is_dll && m.entry != m.base {
            for cb in &m.tls_callbacks {
                attach.push((m.base, *cb, format!("{name} tls")));
            }
            attach.push((m.base, m.entry, name.clone()));
        }
        p.modules.push(m);
        view.cx.data_mut().p = Some(p);
    }
    Ok((exe_entry, exe_base, attach))
}

fn collect_order(host_root: &Path, name: &str, loaded: &mut HashSet<String>, order: &mut Vec<String>) -> Result<()> {
    let key = name.to_lowercase();
    if loaded.contains(&key) {
        return Ok(());
    }
    loaded.insert(key);
    let bytes = std::fs::read(host_root.join(name)).with_context(|| format!("reading {name}"))?;
    let pe = pe::Pe::parse(&bytes).map_err(|e| anyhow!("{name}: {e}"))?;
    for d in pe.imports().map_err(|e| anyhow!("{e}"))? {
        if host_root.join(&d.dll).is_file() {
            collect_order(host_root, &d.dll, loaded, order)?;
        }
    }
    order.push(name.to_string());
    Ok(())
}
