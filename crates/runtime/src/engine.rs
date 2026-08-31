//! The execution engine shared by every host (the wasmtime harness and the browser
//! worker): the dispatcher loop, API calls, faults raised through SEH, and on-demand
//! translation. A host provides a [`Machine`]: guest memory, the CPU-state globals,
//! calling a function-table slot and instantiating a translated module.
//!
//! Control transfer (see `recomp::abi`): translated functions return when the guest
//! executes `ret`, leaving the continuation in `eip`; callers return further when it
//! differs from what they expect. Runtime calls that need the dispatcher (a fault to
//! raise, a context to resume, a stop) record why in [`Engine::ctl`] and set the `ctl`
//! global, which makes translated code return all the way out.

use crate::hle::{ApiError, ApiId, Ctx, Host, Regs};
use crate::mem::Guest;
use crate::process::{Process, RET_TO_HOST};
use recomp::abi::{trap, Global};
use recomp::cfg::FuncInfo;
use std::collections::{HashMap, HashSet, VecDeque};

/// Error text that makes nested host frames unwind to the guest context a handler chose.
pub const UNWIND: &str = "unwind through host frame";
/// Error text of a run that ended through ExitProcess.
pub const EXITED: &str = "process exited";

pub enum Ctl {
    /// Registers in the globals are a new context (SEH continuation).
    Resume,
    /// A fault at `ip` to raise through the guest's SEH chain.
    Fault { code: u32, ip: u32 },
    Stop(String),
}

pub struct ModInfo {
    pub name: String,
    pub base: u32,
    pub size: u32,
    pub functions: HashSet<u32>,
    pub jump_tables: HashMap<u32, Vec<u32>>,
}

/// Host-independent engine state; the host keeps it where callbacks can reach it.
pub struct Engine {
    pub p: Option<Process>,
    /// Guest entry address -> function table index.
    pub funcs: HashMap<u32, u32>,
    /// Table index of a function that does nothing (returned after an API call).
    pub nop_index: u32,
    pub mods: Vec<ModInfo>,
    pub exit: Option<u32>,
    pub ctl: Option<Ctl>,
    pub depth: u32,
    pub translated: usize,
    pub translate_us: u64,
    pub trace: bool,
    pub max_api_calls: u64,
    pub tsc: u64,
    pub bad_functions: HashSet<u32>,
    /// Ring buffer of the most recent dispatcher targets (eip, esp, eax, ebx).
    pub recent: VecDeque<[u32; 4]>,
    pub recent_cap: usize,
    pub dispatches: u64,
    /// x87/MMX instructions run through `rt.fpu`, by mnemonic when `fpu_profile` is on.
    pub fpu_ops: u64,
    pub fpu_profile: Option<HashMap<iced_x86::Mnemonic, u64>>,
    /// Declare guest memory shared in generated modules (browser threads).
    pub shared_memory: bool,
    /// Addresses reported when translated code calls them indirectly (debugging).
    pub watch: Vec<u32>,
    /// Function entries translated with an entry report (debugging).
    pub probes: Vec<u32>,
    /// Translate x87 code without the inline paths (checking them).
    pub no_x87_inline: bool,
}

impl Engine {
    pub fn new(p: Process) -> Self {
        Engine {
            p: Some(p),
            funcs: HashMap::new(),
            nop_index: 0,
            mods: Vec::new(),
            exit: None,
            ctl: None,
            depth: 0,
            translated: 0,
            translate_us: 0,
            trace: false,
            max_api_calls: u64::MAX,
            tsc: 0,
            bad_functions: HashSet::new(),
            recent: VecDeque::new(),
            recent_cap: 0,
            dispatches: 0,
            fpu_ops: 0,
            fpu_profile: None,
            shared_memory: false,
            watch: Vec::new(),
            probes: Vec::new(),
            no_x87_inline: false,
        }
    }

    pub fn process(&self) -> &Process {
        self.p.as_ref().expect("process is lent out")
    }
}

pub trait Machine: Guest {
    fn engine(&mut self) -> &mut Engine;
    fn engine_ref(&self) -> &Engine;
    fn global(&mut self, g: Global) -> u32;
    fn set_global(&mut self, g: Global, v: u32);
    /// Runs the translated function in table slot `idx`. Err only when the host
    /// interrupted it (watchdog, wasm trap).
    fn call_index(&mut self, idx: u32) -> Result<(), String>;
    /// Compiles and instantiates a translated module; returns the table index of each
    /// named export, in order.
    fn instantiate(&mut self, wasm: &[u8], exports: &[String]) -> Result<Vec<u32>, String>;
    /// Wall-clock microseconds (statistics only).
    fn now_us(&self) -> u64;
    fn print(&mut self, s: &str);
    /// Input that arrived from the user since the last call (`frame`: frames shown).
    fn take_input(&mut self, _frame: u64) -> Vec<crate::hle::InputEvent> {
        Vec::new()
    }
}

// ---- registers ---------------------------------------------------------------------------

pub fn read_regs<M: Machine + ?Sized>(m: &mut M) -> Regs {
    Regs {
        eax: m.global(Global::Eax),
        ecx: m.global(Global::Ecx),
        edx: m.global(Global::Edx),
        ebx: m.global(Global::Ebx),
        esp: m.global(Global::Esp),
        ebp: m.global(Global::Ebp),
        esi: m.global(Global::Esi),
        edi: m.global(Global::Edi),
        eip: m.global(Global::Eip),
        fs_base: m.global(Global::FsBase),
    }
}

pub fn write_regs<M: Machine + ?Sized>(m: &mut M, r: &Regs) {
    m.set_global(Global::Eax, r.eax);
    m.set_global(Global::Ecx, r.ecx);
    m.set_global(Global::Edx, r.edx);
    m.set_global(Global::Ebx, r.ebx);
    m.set_global(Global::Esp, r.esp);
    m.set_global(Global::Ebp, r.ebp);
    m.set_global(Global::Esi, r.esi);
    m.set_global(Global::Edi, r.edi);
    m.set_global(Global::Eip, r.eip);
    m.set_global(Global::FsBase, r.fs_base);
}

/// Asks translated code to return to the dispatcher, which then stops with `msg`.
pub fn stop<M: Machine + ?Sized>(m: &mut M, msg: String) {
    let e = m.engine();
    if !matches!(e.ctl, Some(Ctl::Stop(_))) {
        e.ctl = Some(Ctl::Stop(msg));
    }
    m.set_global(Global::Ctl, 1);
}

fn request<M: Machine + ?Sized>(m: &mut M, c: Ctl) {
    let e = m.engine();
    if !matches!(e.ctl, Some(Ctl::Stop(_))) {
        e.ctl = Some(c);
    }
    m.set_global(Global::Ctl, 1);
}

// ---- Host services for the HLE layer ------------------------------------------------------

/// An 8-byte memory operand at address 0 for host-initiated x87 loads and stores.
struct Scratch([u8; 8]);
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

/// The current thread's x87 state, read from its block below the TEB.
pub fn fpu_load<M: Machine + ?Sized>(m: &mut M) -> fpu::Fpu {
    let at = m.global(Global::FsBase).wrapping_sub(fpu::block::OFFSET);
    let mut b = [0u8; fpu::block::SIZE];
    let _ = m.read(at, &mut b);
    fpu::Fpu::from_block(&b)
}

pub fn fpu_store<M: Machine + ?Sized>(m: &mut M, f: &fpu::Fpu) {
    let at = m.global(Global::FsBase).wrapping_sub(fpu::block::OFFSET);
    let mut b = [0u8; fpu::block::SIZE];
    f.to_block(&mut b);
    let _ = m.write(at, &b);
}

/// Guest memory as the x87 model sees it.
struct FpuMem<'a, M: ?Sized>(&'a mut M);
impl<M: Machine + ?Sized> fpu::Mem for FpuMem<'_, M> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        if self.0.read(addr, buf).is_err() {
            buf.fill(0);
        }
    }
    fn write(&mut self, addr: u32, data: &[u8]) {
        let _ = self.0.write(addr, data);
    }
}

impl<M: Machine> Host for M {
    fn call_guest(&mut self, p: &mut Process, regs: &mut Regs, addr: u32, args: &[u32]) -> Result<u32, String> {
        let depth = {
            let e = self.engine();
            e.depth += 1;
            e.depth
        };
        let marker = RET_TO_HOST + 4 * depth;
        // The callee's frame goes below everything the API has on the stack.
        let mut esp = (regs.esp - 0x80) & !0xF;
        for a in args.iter().rev() {
            esp -= 4;
            self.write_u32(esp, *a).map_err(|e| e.to_string())?;
        }
        esp -= 4;
        self.write_u32(esp, marker).map_err(|e| e.to_string())?;
        let frame_top = esp + 4 + 4 * args.len() as u32;
        let mut callee = *regs;
        callee.esp = esp;
        callee.eip = addr;
        write_regs(self, &callee);
        // Lend the process to the nested dispatcher.
        let owned = std::mem::replace(p, Process::empty());
        self.engine().p = Some(owned);
        let r = run_until(self, addr, marker, Some(frame_top));
        {
            let e = self.engine();
            e.depth -= 1;
            if let Some(back) = e.p.take() {
                *p = back;
            }
        }
        r?;
        let after = read_regs(self);
        // Registers come back as the callee left them; the API's own stack pointer
        // stays, since the callee popped its frame.
        regs.eax = after.eax;
        regs.ecx = after.ecx;
        regs.edx = after.edx;
        regs.ebx = after.ebx;
        regs.esi = after.esi;
        regs.edi = after.edi;
        regs.ebp = after.ebp;
        Ok(after.eax)
    }

    fn request_exit(&mut self, code: u32) {
        self.engine().exit = Some(code);
        stop(self, EXITED.into());
    }

    fn fpu_push(&mut self, v: f64) {
        let mut s = Scratch(v.to_bits().to_le_bytes());
        let mut f = fpu_load(self);
        f.exec(iced_x86::Mnemonic::Fld, 0, (iced_x86::MemorySize::Float64 as u32) << 8, 0, 0, &mut s);
        fpu_store(self, &f);
    }

    fn poll_input(&mut self, frame: u64) -> Vec<crate::hle::InputEvent> {
        self.take_input(frame)
    }

    fn fpu_pop(&mut self) -> f64 {
        let mut s = Scratch([0; 8]);
        let mut f = fpu_load(self);
        f.exec(iced_x86::Mnemonic::Fstp, 0, (iced_x86::MemorySize::Float64 as u32) << 8, 0, 0, &mut s);
        fpu_store(self, &f);
        f64::from_bits(u64::from_le_bytes(s.0))
    }
}

/// Runs `f` with the process checked out of the engine and a `Ctx` built on it.
pub fn with_ctx<M: Machine, R>(m: &mut M, regs: &mut Regs, id: ApiId, f: impl FnOnce(&mut Ctx) -> R) -> Result<R, String> {
    let mut p = m.engine().p.take().ok_or("process not available (nested host call without lending)")?;
    let r = {
        let mut ctx = Ctx { host: m, regs, p: &mut p, id };
        f(&mut ctx)
    };
    m.engine().p = Some(p);
    Ok(r)
}

// ---- API calls -----------------------------------------------------------------------------

fn format_args<M: Machine>(m: &M, esp: u32, n: u32) -> String {
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!("{:#x}", m.read_u32(esp + 4 + 4 * i).unwrap_or(0)));
    }
    s
}

/// Runs the API behind a stub. Problems are reported through `ctl`.
pub fn call_api<M: Machine>(m: &mut M, id: ApiId) {
    let mut regs = read_regs(m);
    let (trace, depth, max_calls) = {
        let e = m.engine_ref();
        (e.trace, e.depth, e.max_api_calls)
    };
    let (name, func, nargs, calls) = {
        let Some(p) = m.engine().p.as_mut() else { return stop(m, "process not available".into()) };
        p.api_calls += 1;
        if !p.registry.idle_ok.contains(&id.0) {
            p.idle_streak = 0;
        }
        let e = &p.registry.entries[id.0 as usize];
        (format!("{}!{}", e.dll, e.name), e.func, e.nargs, p.api_calls)
    };
    if calls > max_calls {
        return stop(m, format!("API call budget exhausted ({max_calls})"));
    }
    let ra = m.read_u32(regs.esp).unwrap_or(0);
    if trace {
        let args = format_args(m, regs.esp, if nargs > 0 { nargs.min(8) } else { 4 });
        let line = format!("{:w$}{name}({args}) from {ra:#x}", "", w = (depth * 2) as usize);
        m.print(&line);
    }
    let Some(f) = func else {
        let _ = with_ctx(m, &mut regs, id, |c| c.p.unimplemented.push(name.clone()));
        return stop(m, format!("unimplemented API {name} called from {ra:#x}"));
    };
    let result = match with_ctx(m, &mut regs, id, |c| f(c)) {
        Ok(r) => r,
        Err(e) => return stop(m, e),
    };
    match result {
        Ok(ret) => {
            if ret.resume {
                if trace {
                    let line = format!("{:w$}  -> resume at {:#x}", "", regs.eip, w = (depth * 2) as usize);
                    m.print(&line);
                }
                write_regs(m, &regs);
                request(m, Ctl::Resume);
            } else {
                regs.eax = ret.eax;
                if let Some(edx) = ret.edx {
                    regs.edx = edx;
                }
                regs.esp += 4 + ret.pop;
                regs.eip = ra;
                if trace {
                    let line = format!("{:w$}  = {:#x}", "", ret.eax, w = (depth * 2) as usize);
                    m.print(&line);
                }
                write_regs(m, &regs);
            }
        }
        // A handler continued in a frame outside the callback this API made: the
        // globals already hold that context.
        Err(ApiError(msg)) if msg == UNWIND => request(m, Ctl::Resume),
        Err(ApiError(msg)) if msg == EXITED => stop(m, msg),
        Err(ApiError(msg)) => stop(m, format!("{name}: {msg}")),
    }
}

/// Raises a fault through the guest's SEH chain with the current registers as context.
pub fn raise_fault<M: Machine>(m: &mut M, code: u32, ip: u32) -> Result<(), String> {
    let mut regs = read_regs(m);
    regs.eip = ip;
    let id = m.engine_ref().process().registry.lookup("kernel32.dll", "RaiseException").ok_or("RaiseException not registered")?;
    let ctx_regs = regs;
    let r = with_ctx(m, &mut regs, id, |c| crate::hle::kernel32::dispatch_exception(c, code, 0, ip, &[], &ctx_regs))?;
    match r {
        Ok(ret) if ret.resume => {
            write_regs(m, &regs);
            Ok(())
        }
        Ok(_) => Err(format!("exception {code:#x} at {ip:#x}: handler returned without a context")),
        Err(ApiError(msg)) => Err(msg),
    }
}

// ---- runtime services imported by translated code ------------------------------------------

/// `rt.resolve`: table index of the function at `addr`, translating it if needed. An
/// HLE stub runs its API and yields the no-op function.
pub fn rt_resolve<M: Machine>(m: &mut M, addr: u32) -> u32 {
    if !m.engine_ref().watch.is_empty() && m.engine_ref().watch.contains(&addr) {
        let (esp, ra) = (m.global(Global::Esp), 0);
        let ra = m.read_u32(esp).unwrap_or(ra);
        let frame = m.engine_ref().p.as_ref().map(|p| p.frames_presented);
        m.print(&format!("watch: {addr:#x} called (return {ra:#x}, frame {frame:?})"));
    }
    match resolve_addr(m, addr) {
        Ok(i) => i,
        Err(e) => {
            stop(m, e);
            m.engine_ref().nop_index
        }
    }
}

pub fn rt_trap<M: Machine>(m: &mut M, kind: i32, arg: u32) {
    let ip = m.global(Global::Eip);
    match kind {
        trap::INT3 => m.print(&format!("int3 at {arg:#x} (ignored)")),
        trap::PROBE => {
            let r = read_regs(m);
            let ra = m.read_u32(r.esp).unwrap_or(0);
            let args: Vec<String> = (1..5).map(|i| format!("{:#x}", m.read_u32(r.esp + 4 * i).unwrap_or(0))).collect();
            m.print(&format!(
                "probe {arg:#x}: from {ra:#x} eax={:#x} ecx={:#x} edx={:#x} ebx={:#x} esi={:#x} edi={:#x} stack [{}]",
                r.eax,
                r.ecx,
                r.edx,
                r.ebx,
                r.esi,
                r.edi,
                args.join(", ")
            ));
        }
        trap::DIVIDE_ERROR => request(m, Ctl::Fault { code: 0xC000_0094, ip: arg }),
        trap::UD2 => request(m, Ctl::Fault { code: 0xC000_001D, ip: arg }),
        trap::INT => stop(m, format!("int {arg:#x} at {ip:#x}")),
        trap::HLT => stop(m, format!("hlt at {arg:#x}")),
        trap::NO_TARGET => stop(m, format!("branch to an unresolvable target at {arg:#x}")),
        trap::UNIMPLEMENTED => stop(m, format!("unimplemented instruction at {arg:#x}")),
        _ => stop(m, format!("trap {kind} at {arg:#x}")),
    }
}

/// `rt.fpu`: one x87/MMX instruction. A pending unmasked exception becomes a fault
/// (#MF) raised once the translated code has stored its registers.
pub fn rt_fpu<M: Machine>(m: &mut M, code: i32, a: u32, b: u32, ip: u32, fop: u32) -> i32 {
    let mn = iced_x86::Code::try_from(code as usize).map(|c| c.mnemonic()).unwrap_or(iced_x86::Mnemonic::INVALID);
    let mut f = fpu_load(m);
    if f.fault_pending(mn) {
        let (sw, cw) = (f.status_word(), f.control_word());
        let pending = sw & !cw & 0x3F;
        let status = if pending & 1 != 0 {
            0xC000_0090 // invalid operation
        } else if pending & 4 != 0 {
            0xC000_008E // divide by zero
        } else if pending & 8 != 0 {
            0xC000_0091 // overflow
        } else if pending & 0x10 != 0 {
            0xC000_0093 // underflow
        } else if pending & 2 != 0 {
            0xC000_008D // denormal
        } else {
            0xC000_008F // inexact
        };
        // The kernel reports the fault with the exception bits cleared from the live
        // state (handlers see them in the saved context).
        f.set_status_word(sw & !0x80BF);
        fpu_store(m, &f);
        request(m, Ctl::Fault { code: status, ip });
        return 0;
    }
    let e = m.engine();
    e.fpu_ops += 1;
    if let Some(h) = e.fpu_profile.as_mut() {
        *h.entry(mn).or_default() += 1;
    }
    let r = f.exec(mn, a, b, ip, fop as u16, &mut FpuMem(m));
    fpu_store(m, &f);
    r
}

pub fn cpuid_regs(leaf: u32, _sub: u32) -> (u32, u32, u32, u32) {
    match leaf {
        0 => (1, 0x756E_6547, 0x6C65_746E, 0x4965_6E69),
        1 => (0x0001_06A5, 0x0001_0800, 0, 0x0780_8111),
        0x8000_0000 => (0x8000_0000, 0, 0, 0),
        _ => (0, 0, 0, 0),
    }
}

pub fn rt_cpuid<M: Machine>(m: &mut M) {
    let (leaf, sub) = (m.global(Global::Eax), m.global(Global::Ecx));
    let (a, b, c, d) = cpuid_regs(leaf, sub);
    m.set_global(Global::Eax, a);
    m.set_global(Global::Ebx, b);
    m.set_global(Global::Ecx, c);
    m.set_global(Global::Edx, d);
}

pub fn rt_rdtsc<M: Machine>(m: &mut M) {
    let us = m.engine_ref().p.as_ref().map(|p| p.clock_us).unwrap_or(0);
    let e = m.engine();
    // 3 GHz against the virtual clock, strictly increasing.
    e.tsc = e.tsc.max(us * 3000) + 1000;
    let v = e.tsc;
    m.set_global(Global::Eax, v as u32);
    m.set_global(Global::Edx, (v >> 32) as u32);
}

// ---- translation -----------------------------------------------------------------------------

struct CodeMem<'a, M: ?Sized>(&'a M);
impl<M: Machine + ?Sized> recomp::Memory for CodeMem<'_, M> {
    fn read(&self, va: u32, buf: &mut [u8]) -> bool {
        self.0.read(va, buf).is_ok()
    }
}

/// Registers newly loaded modules (discovery runs once per image).
pub fn sync_modules<M: Machine>(m: &mut M) {
    let new: Vec<(String, u32, u32, Vec<u8>)> = {
        let e = m.engine_ref();
        let Some(p) = e.p.as_ref() else { return };
        p.modules.iter().filter(|md| !e.mods.iter().any(|k| k.base == md.base)).map(|md| (md.name.clone(), md.base, md.size, md.file.clone())).collect()
    };
    for (name, base, size, file) in new {
        let t = m.now_us();
        let mut functions = HashSet::new();
        let mut jump_tables = HashMap::new();
        match pe::Pe::parse(&file) {
            Ok(pe) => {
                let d = x86::discover(&pe);
                for f in &d.function_list {
                    functions.insert(base + f.rva);
                }
                for jt in &d.jump_tables {
                    let targets = (0..jt.entries).map(|i| m.read_u32(base + jt.table_rva + 4 * i).unwrap_or(0)).collect();
                    jump_tables.insert(base + jt.site_rva, targets);
                }
                let dt = (m.now_us() - t) as f64 / 1e6;
                if !m.engine_ref().trace {
                    m.print(&format!("[{name}] discovery: {} functions, {} jump tables ({dt:.1}s)", functions.len(), jump_tables.len()));
                }
            }
            Err(e) => m.print(&format!("[{name}] discovery failed: {e}")),
        }
        m.engine().mods.push(ModInfo { name, base, size, functions, jump_tables });
    }
}

/// Translates the functions starting at `entries` into one module and registers them.
/// Returns the entries that could not be translated.
pub fn translate_batch<M: Machine>(m: &mut M, entries: &[u32]) -> Result<Vec<u32>, String> {
    let t = m.now_us();
    let shared = m.engine_ref().shared_memory;
    let funcs = std::mem::take(&mut m.engine().funcs);
    let mods = std::mem::take(&mut m.engine().mods);
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    let out = {
        let mem = CodeMem(&*m);
        let probes = m.engine_ref().probes.clone();
        let no_x87_inline = m.engine_ref().no_x87_inline;
        let mut tr = recomp::Translator::new(&mem, recomp::Options { shared_memory: shared, probes, no_x87_inline });
        for &entry in entries {
            let md = mods.iter().find(|md| entry >= md.base && entry < md.base + md.size);
            let empty_jt = HashMap::new();
            let jt = md.map(|md| &md.jump_tables).unwrap_or(&empty_jt);
            let is_fn = |a: u32| funcs.contains_key(&a) || md.map(|md| md.functions.contains(&a)).unwrap_or(false);
            let info = FuncInfo { jump_tables: jt, is_function: &is_fn };
            if let Err(e) = tr.add_function(entry, &info) {
                errors.push(format!("translate {entry:#x}: {e}"));
                failed.push(entry);
            }
        }
        tr.finish()
    };
    m.engine().funcs = funcs;
    m.engine().mods = mods;
    for e in errors {
        m.print(&e);
    }
    if out.functions.is_empty() {
        return Ok(failed);
    }
    let names: Vec<String> = out.functions.iter().map(|(_, n)| n.clone()).collect();
    let indices = match m.instantiate(&out.wasm, &names) {
        Ok(ix) => ix,
        Err(e) => {
            if entries.len() == 1 {
                return Err(format!("wasm for function {:#x} rejected: {e}", entries[0]));
            }
            // Bisect to isolate the offending function.
            let (a, b) = entries.split_at(entries.len() / 2);
            failed.extend(translate_batch(m, a)?);
            failed.extend(translate_batch(m, b)?);
            return Ok(failed);
        }
    };
    let now = m.now_us();
    let e = m.engine();
    for ((entry, _), idx) in out.functions.iter().zip(indices) {
        e.funcs.insert(*entry, idx);
    }
    e.translated += out.functions.len();
    e.translate_us += now - t;
    if e.trace {
        let lines: Vec<String> = out.unimplemented.iter().map(|(a, t)| format!("unimplemented instruction at {a:#x}: {t}")).collect();
        for l in lines {
            m.print(&l);
        }
    }
    Ok(failed)
}

/// Translates every function discovery found in the loaded modules.
pub fn translate_all<M: Machine>(m: &mut M, chunk: usize) -> Result<usize, String> {
    sync_modules(m);
    let mut entries: Vec<u32> = m.engine_ref().mods.iter().flat_map(|md| md.functions.iter().copied()).filter(|a| !m.engine_ref().funcs.contains_key(a)).collect();
    entries.sort_unstable();
    let mut failed = 0;
    for c in entries.chunks(chunk.max(1)) {
        failed += translate_batch(m, c)?.len();
    }
    Ok(failed)
}

pub fn resolve_addr<M: Machine>(m: &mut M, addr: u32) -> Result<u32, String> {
    if let Some(&i) = m.engine_ref().funcs.get(&addr) {
        return Ok(i);
    }
    if let Some(id) = Process::stub_id(addr) {
        call_api(m, id);
        return Ok(m.engine_ref().nop_index);
    }
    if (RET_TO_HOST..RET_TO_HOST + 0x1000).contains(&addr) {
        return Err(format!("guest returned to a host frame marker {addr:#x} that is not the innermost one"));
    }
    if m.engine_ref().bad_functions.contains(&addr) {
        return Err(format!("function {addr:#x} cannot be translated"));
    }
    sync_modules(m);
    let failed = translate_batch(m, &[addr])?;
    if !failed.is_empty() {
        m.engine().bad_functions.insert(addr);
        return Err(format!("cannot translate function at {addr:#x}"));
    }
    m.engine_ref().funcs.get(&addr).copied().ok_or_else(|| format!("translation of {addr:#x} produced no function"))
}

// ---- dispatcher ---------------------------------------------------------------------------------

/// Runs guest code from `addr` until it returns to `marker`. `frame_top` bounds the
/// stack of a nested call: a handler continuing above it leaves this call through
/// the host frames that made it.
pub fn run_until<M: Machine>(m: &mut M, addr: u32, marker: u32, frame_top: Option<u32>) -> Result<(), String> {
    let mut eip = addr;
    loop {
        {
            let cap = m.engine_ref().recent_cap;
            let rec = if cap > 0 { Some([eip, m.global(Global::Esp), m.global(Global::Eax), m.global(Global::Ebx)]) } else { None };
            let e = m.engine();
            e.dispatches += 1;
            if let Some(r) = rec {
                if e.recent.len() == cap {
                    e.recent.pop_front();
                }
                e.recent.push_back(r);
            }
        }
        let idx = resolve_addr(m, eip)?;
        m.call_index(idx)?;
        // The guest left this nested call without returning to it (Delphi's exception
        // handlers jump into the `except` block on the handler frame's stack): unwind
        // the host frames that made the call and continue in the outer dispatcher.
        if let Some(top) = frame_top {
            if m.global(Global::Esp) > top && m.global(Global::Ctl) == 0 {
                return Err(UNWIND.into());
            }
        }
        if m.global(Global::Ctl) != 0 {
            m.set_global(Global::Ctl, 0);
            match m.engine().ctl.take() {
                Some(Ctl::Stop(msg)) => {
                    // Keep unwinding outer frames too.
                    m.engine().ctl = Some(Ctl::Stop(msg.clone()));
                    return Err(msg);
                }
                Some(Ctl::Fault { code, ip }) => raise_fault(m, code, ip)?,
                Some(Ctl::Resume) | None => {}
            }
            if let Some(top) = frame_top {
                if m.global(Global::Esp) > top {
                    return Err(UNWIND.into());
                }
            }
        }
        eip = m.global(Global::Eip);
        if eip == marker {
            return Ok(());
        }
    }
}

/// Calls `addr` with stdcall `args` from the host (DllMain, the entry point).
pub fn call_from_host<M: Machine>(m: &mut M, addr: u32, args: &[u32]) -> Result<(), String> {
    let mut esp = m.global(Global::Esp);
    for a in args.iter().rev() {
        esp -= 4;
        m.write_u32(esp, *a).map_err(|e| e.to_string())?;
    }
    esp -= 4;
    m.write_u32(esp, RET_TO_HOST).map_err(|e| e.to_string())?;
    m.set_global(Global::Esp, esp);
    m.set_global(Global::Eip, addr);
    let r = run_until(m, addr, RET_TO_HOST, None);
    // A pending stop stays visible to the caller; clear it for the next call.
    m.engine().ctl = None;
    r
}

// ---- process start ---------------------------------------------------------------------------

pub struct Started {
    pub entry: u32,
    /// (module base, DllMain or TLS callback, label) to run before the entry point.
    pub attach: Vec<(u32, u32, String)>,
}

/// Maps the exe and the game DLLs it imports (dependencies first), lays out the main
/// thread and static TLS, and sets the initial registers. Files come from the
/// process's VFS (game directory).
pub fn start<M: Machine>(m: &mut M, exe: &str) -> Result<Started, String> {
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let read = |m: &M, name: &str| m.engine_ref().process().fs.read_all(name);
    {
        let r = |name: &str| read(m, name);
        collect_order(exe, &r, &mut seen, &mut order)?;
    }
    let mut attach = Vec::new();
    let (mut entry, mut image_base) = (0, 0);
    for name in order {
        let bytes = read(m, &name).ok_or_else(|| format!("reading {name}"))?;
        let guest_path = format!("C:\\game\\{name}");
        let mut p = m.engine().p.take().ok_or("process not available")?;
        let loaded = {
            let req = crate::LoadRequest { name: &name, path: &guest_path, bytes };
            let mut resolver = crate::StubResolver { registry: &mut p.registry, modules: &p.modules };
            crate::loader::load(req, &mut p.vm, m, &mut resolver)
        };
        let md = match loaded {
            Ok(md) => md,
            Err(e) => {
                m.engine().p = Some(p);
                return Err(e);
            }
        };
        m.print(&format!("loaded {name} at {:#x} ({} KB, entry {:#x}, {} imports)", md.base, md.size / 1024, md.entry, md.imports.len()));
        if name.eq_ignore_ascii_case(exe) {
            entry = md.entry;
            image_base = md.base;
        } else if md.is_dll && md.entry != md.base {
            for cb in &md.tls_callbacks {
                attach.push((md.base, *cb, format!("{name} tls")));
            }
            attach.push((md.base, md.entry, name.clone()));
        }
        p.modules.push(md);
        m.engine().p = Some(p);
    }
    let mut p = m.engine().p.take().ok_or("process not available")?;
    let r = p.setup_main_thread(m, image_base).and_then(|r| p.setup_static_tls(m).map(|_| r));
    m.engine().p = Some(p);
    let (teb, stack_top) = r?;
    m.set_global(Global::FsBase, teb);
    m.set_global(Global::Mxcsr, 0x1F80);
    let regs = Regs { eax: 0, ecx: 0, edx: 0, ebx: 0, esp: stack_top - 0x100, ebp: 0, esi: 0, edi: 0, eip: entry, fs_base: teb };
    write_regs(m, &regs);
    Ok(Started { entry, attach })
}

fn collect_order(name: &str, read: &dyn Fn(&str) -> Option<Vec<u8>>, seen: &mut HashSet<String>, order: &mut Vec<String>) -> Result<(), String> {
    if !seen.insert(name.to_lowercase()) {
        return Ok(());
    }
    let bytes = read(name).ok_or_else(|| format!("reading {name}"))?;
    let pe = pe::Pe::parse(&bytes).map_err(|e| format!("{name}: {e}"))?;
    for d in pe.imports().map_err(|e| e.to_string())? {
        if read(&d.dll).is_some() {
            collect_order(&d.dll, read, seen, order)?;
        }
    }
    order.push(name.to_string());
    Ok(())
}

/// Runs DllMain of the statically imported game DLLs, then the entry point. Ok also
/// when the run ended through ExitProcess or the frame limit.
pub fn run_process<M: Machine>(m: &mut M, started: &Started) -> Result<(), String> {
    for (base, dll_entry, name) in &started.attach {
        m.print(&format!("DllMain({name}) at {dll_entry:#x}"));
        call_from_host(m, *dll_entry, &[*base, 1, 0])?;
    }
    match call_from_host(m, started.entry, &[]) {
        Err(e) if e == EXITED || e.ends_with(crate::hle::FRAME_LIMIT) => Ok(()),
        r => r,
    }
}

/// A module exporting `nop: () -> ()`, the function `resolve` returns after an API call.
pub fn nop_module() -> Vec<u8> {
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
    let mut md = Module::new();
    md.section(&types);
    md.section(&funcs);
    md.section(&exports);
    md.section(&code);
    md.finish()
}

/// Fingerprint of registers and every non-zero 64 KiB chunk of guest memory below
/// `limit`: equal hashes mean equal machine states.
pub fn state_hash<M: Machine>(m: &mut M, limit: u32) -> (u64, u32) {
    let regs = read_regs(m);
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    let mix = |h: &mut u64, bytes: &[u8]| {
        for &b in bytes {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x0100_0000_01B3);
        }
    };
    for r in [regs.eax, regs.ecx, regs.edx, regs.ebx, regs.esp, regs.ebp, regs.esi, regs.edi, regs.eip] {
        mix(&mut h, &r.to_le_bytes());
    }
    // The x87 state by value (its FXSAVE image): the block's bytes depend on which
    // path (inline or rt.fpu) last wrote it, its contents do not.
    let mut fx = [0u8; 512];
    fpu_load(m).write_fxsave(&mut fx);
    mix(&mut h, &fx[..160]);
    let block = regs.fs_base.wrapping_sub(fpu::block::OFFSET);
    let mut chunks = 0;
    let mut buf = vec![0u8; 65536];
    let mut addr: u64 = 0;
    while addr < limit as u64 {
        if m.read(addr as u32, &mut buf).is_err() {
            break;
        }
        if let Some(off) = (block as u64).checked_sub(addr).filter(|&o| o < 65536) {
            let off = off as usize;
            buf[off..(off + fpu::block::SIZE).min(65536)].fill(0);
        }
        if buf.iter().any(|&b| b != 0) {
            mix(&mut h, &((addr / 65536) as u32).to_le_bytes());
            mix(&mut h, &buf);
            chunks += 1;
        }
        addr += 65536;
    }
    (h, chunks)
}
