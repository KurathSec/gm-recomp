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
use crate::process::{ThreadCtx, Process, RET_TO_HOST};
use recomp::abi::{self, trap, Global};
use recomp::cfg::FuncInfo;
use std::collections::VecDeque;
use crate::fasthash::{HashMap, HashSet};

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
    /// The running secondary thread blocked: back to the scheduler.
    Yield,
}

/// Dispatches a secondary thread may run before it is preempted (busy waits).
const SLICE_BUDGET: u64 = 200_000;

pub struct ModInfo {
    pub name: String,
    pub base: u32,
    pub size: u32,
    pub functions: HashSet<u32>,
    /// The same, in address order.
    pub sorted: Vec<u32>,
    /// As discovery returns them (only looked up).
    pub jump_tables: std::collections::HashMap<u32, Vec<u32>>,
    /// FNV-1a of the executable sections' bytes: the same for every game built with
    /// the same runner (a GM8 game's data follows the sections), so translation
    /// profiles can be shared between them.
    pub code_id: u32,
}

pub use crate::fasthash::AddrHash;

/// Host-independent engine state; the host keeps it where callbacks can reach it.
pub struct Engine {
    /// Boxed: it is lent out at every API call (4 KB to move otherwise).
    pub p: Option<Box<Process>>,
    /// Guest entry address -> function table index (looked up at every dispatch).
    pub funcs: HashMap<u32, u32, AddrHash>,
    /// Table index of a function that does nothing (returned after an API call).
    pub nop_index: u32,
    pub mods: Vec<ModInfo>,
    pub exit: Option<u32>,
    pub ctl: Option<Ctl>,
    pub depth: u32,
    pub translated: usize,
    /// Modules instantiated for translated code.
    pub modules: usize,
    pub translate_us: u64,
    pub trace: bool,
    pub max_api_calls: u64,
    pub tsc: u64,
    pub bad_functions: HashSet<u32>,
    /// Functions discovery found that nothing has called yet, for `translate_idle`
    /// (popped from the end) with their sizes (bytes up to the next one), and its
    /// estimate of the time a byte takes (ns).
    pub idle_queue: Option<Vec<(u32, u32)>>,
    pub idle_ns_per_byte: u64,
    /// Fixed cost of an idle batch (ns): a module's compile and instantiate cost a
    /// share no matter its size, which a rate per byte alone overstated for small
    /// batches, so few fit in a wait.
    pub idle_fixed_ns: u64,
    /// Recent idle batches (bytes, ns), which the two are fitted to.
    pub idle_samples: std::collections::VecDeque<(u64, u64)>,
    /// Functions translated code calls directly that are not translated yet: likely
    /// next, so `translate_idle` takes them first (popped from the end).
    pub idle_next: Vec<u32>,
    /// Functions earlier sessions translated when they ran, with a hash of everything
    /// they were translated from (`recomp::Translator::code_hash`) and the frame they
    /// were first needed in, in reverse order of need: translated before the start
    /// (`profile_prefetch`), after the first frame or first while the game waits, and
    /// only when that code is still the same.
    pub profile: Vec<(u32, u32, u32)>,
    /// Functions translated this session when they ran, with their code hash, for
    /// the next session's profile (those already in it left out).
    pub demand: Vec<(u32, u32, u32)>,
    pub profile_known: HashSet<u32>,
    /// Frames presented as of the last input poll (readable while the process is lent
    /// out).
    pub frames_seen: u64,
    /// The profile's burst ran (see `translate_idle`).
    pub profile_burst: bool,
    /// The functions the last `translate_batch` translated, with their code hashes
    /// (`recomp::Translator::code_hash`).
    pub last_batch: Vec<(u32, u32)>,
    /// Functions translated ahead by `translate_idle`, and its calls.
    pub idle_done: usize,
    pub idle_calls: usize,
    /// Ring buffer of the most recent dispatcher targets (eip, esp, eax, ebx).
    pub recent: VecDeque<[u32; 4]>,
    pub recent_cap: usize,
    pub dispatches: u64,
    /// API calls so far (as the process's count, but readable while it is lent out).
    pub api_calls: u64,
    /// Calls per API id, when wanted (debugging; `GMR_API_COUNT` in the harness).
    pub api_hist: Option<Vec<u64>>,
    /// `rt.resolve` calls that found a translated function (indirect and cross-module
    /// calls).
    pub resolves: u64,
    /// x87/MMX instructions run through `rt.fpu`, by instruction form and address when
    /// `fpu_profile` is on.
    pub fpu_ops: u64,
    pub fpu_profile: Option<HashMap<(iced_x86::Code, u32), u64>>,
    /// Declare guest memory shared in generated modules (browser threads).
    pub shared_memory: bool,
    /// Addresses reported when translated code calls them indirectly (debugging).
    pub watch: Vec<u32>,
    /// Function entries translated with an entry report (debugging).
    pub probes: Vec<u32>,
    /// Translate x87 code without the inline paths (checking them).
    pub no_x87_inline: bool,
    /// Do not inline small leaf callees (checking the inlining).
    pub no_inline: bool,
    /// Translated code counts events in `abi::COUNTERS` (debugging).
    pub count: bool,
    pub count_blocks: bool,
    /// Host time spent running the other guest threads (while the current one waits).
    pub thread_us: u64,
    /// Host time spent translating ahead (`translate_idle`).
    pub idle_us: u64,
    /// Callees considered for inlining, kept from one translation to the next.
    pub inline_cache: Option<recomp::fasthash::HashMap<(u32, u8), Option<std::rc::Rc<recomp::cfg::Cfg>>>>,
    /// Print each input event as the guest takes it (`input <frame> <clock> ...`), so
    /// a session can be replayed (`harness run --replay`).
    pub monitor: bool,
}

impl Engine {
    pub fn new(p: Process) -> Self {
        Engine {
            p: Some(Box::new(p)),
            funcs: HashMap::default(),
            nop_index: 0,
            mods: Vec::new(),
            exit: None,
            ctl: None,
            depth: 0,
            translated: 0,
            modules: 0,
            translate_us: 0,
            trace: false,
            max_api_calls: u64::MAX,
            tsc: 0,
            bad_functions: HashSet::default(),
            idle_queue: None,
            idle_ns_per_byte: 0,
            idle_fixed_ns: 500_000,
            idle_samples: std::collections::VecDeque::new(),
            idle_next: Vec::new(),
            profile: Vec::new(),
            demand: Vec::new(),
            profile_known: HashSet::default(),
            frames_seen: 0,
            profile_burst: false,
            last_batch: Vec::new(),
            idle_done: 0,
            idle_calls: 0,
            recent: VecDeque::new(),
            recent_cap: 0,
            dispatches: 0,
            resolves: 0,
            fpu_ops: 0,
            fpu_profile: None,
            shared_memory: false,
            watch: Vec::new(),
            probes: Vec::new(),
            no_x87_inline: false,
            no_inline: false,
            count: false,
            count_blocks: false,
            thread_us: 0,
            idle_us: 0,
            api_calls: 0,
            api_hist: None,
            inline_cache: None,
            monitor: false,
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
    /// The general registers, eip and the FS base (a host may move them at once).
    fn regs(&mut self) -> Regs {
        Regs {
            eax: self.global(Global::Eax),
            ecx: self.global(Global::Ecx),
            edx: self.global(Global::Edx),
            ebx: self.global(Global::Ebx),
            esp: self.global(Global::Esp),
            ebp: self.global(Global::Ebp),
            esi: self.global(Global::Esi),
            edi: self.global(Global::Edi),
            eip: self.global(Global::Eip),
            fs_base: self.global(Global::FsBase),
        }
    }
    fn set_regs(&mut self, r: &Regs) {
        self.set_global(Global::Eax, r.eax);
        self.set_global(Global::Ecx, r.ecx);
        self.set_global(Global::Edx, r.edx);
        self.set_global(Global::Ebx, r.ebx);
        self.set_global(Global::Esp, r.esp);
        self.set_global(Global::Ebp, r.ebp);
        self.set_global(Global::Esi, r.esi);
        self.set_global(Global::Edi, r.edi);
        self.set_global(Global::Eip, r.eip);
        self.set_global(Global::FsBase, r.fs_base);
    }
    /// XMM register `i` (thread switches save and restore them).
    fn xmm(&mut self, i: u32) -> u128;
    fn set_xmm(&mut self, i: u32, v: u128);
    /// Runs the translated function in table slot `idx`. Err only when the host
    /// interrupted it (watchdog, wasm trap).
    fn call_index(&mut self, idx: u32) -> Result<(), String>;
    /// Compiles and instantiates a translated module; returns the table index of each
    /// named export, in order.
    fn instantiate(&mut self, wasm: &[u8], exports: &[String]) -> Result<Vec<u32>, String>;
    /// Wall-clock microseconds (statistics only).
    fn now_us(&self) -> u64;
    fn print(&mut self, s: &str);
    /// Input that arrived from the user since the last call (`frame`: frames shown,
    /// `clock_us`: the guest's clock).
    fn take_input(&mut self, _frame: u64, _clock_us: u64) -> Vec<crate::hle::InputEvent> {
        Vec::new()
    }
    /// A General MIDI SoundFont, once available (the first call may start loading it).
    fn soundfont(&mut self) -> Option<Vec<u8>> {
        None
    }
    /// See `Host::pace`.
    fn pace(&mut self, _clock_us: u64) {}
}

// ---- registers ---------------------------------------------------------------------------

pub fn read_regs<M: Machine + ?Sized>(m: &mut M) -> Regs {
    m.regs()
}

pub fn write_regs<M: Machine + ?Sized>(m: &mut M, r: &Regs) {
    m.set_regs(r)
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
        p.nest += 1;
        let owned = std::mem::replace(p, Process::empty());
        self.engine().p = Some(Box::new(owned));
        let r = run_until(self, addr, marker, Some(frame_top));
        {
            let e = self.engine();
            e.depth -= 1;
            if let Some(back) = e.p.take() {
                *p = *back;
            }
        }
        p.nest -= 1;
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

    fn fpu_state(&mut self) -> fpu::Fpu {
        fpu_load(self)
    }

    fn yield_thread(&mut self) {
        request(self, Ctl::Yield);
    }

    fn run_threads(&mut self, p: &mut Process) {
        // (timed only when a thread will run: this is asked a thousand times a frame,
        // and reading the host clock is a call out to it)
        if !threads_ready(p) {
            return;
        }
        let t = self.now_us();
        run_threads(self, p);
        let dt = self.now_us() - t;
        self.engine().thread_us += dt;
    }

    fn soundfont(&mut self) -> Option<Vec<u8>> {
        Machine::soundfont(self)
    }

    fn pace(&mut self, clock_us: u64) {
        Machine::pace(self, clock_us)
    }

    fn set_fpu_state(&mut self, f: &fpu::Fpu) {
        fpu_store(self, f)
    }

    fn mxcsr(&mut self) -> u32 {
        self.global(Global::Mxcsr)
    }

    fn set_mxcsr(&mut self, v: u32) {
        self.set_global(Global::Mxcsr, v)
    }

    fn poll_input(&mut self, frame: u64, clock_us: u64) -> Vec<crate::hle::InputEvent> {
        self.engine().frames_seen = frame;
        self.take_input(frame, clock_us)
    }

    fn monitoring(&self) -> bool {
        self.engine_ref().monitor
    }

    fn monitor(&mut self, line: &str) {
        self.print(line);
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
    m.engine().api_calls += 1;
    if let Some(h) = m.engine().api_hist.as_mut() {
        if h.len() <= id.0 as usize {
            h.resize(id.0 as usize + 1, 0);
        }
        h[id.0 as usize] += 1;
    }
    let (func, nargs, pop, calls) = {
        let Some(p) = m.engine().p.as_mut() else { return stop(m, "process not available".into()) };
        p.api_calls += 1;
        if !p.registry.idle_ok.contains(&id.0) {
            p.idle_streak = 0;
        }
        let e = &p.registry.entries[id.0 as usize];
        (e.func, e.nargs, e.pop, p.api_calls)
    };
    // "dll!name", for traces and errors only
    let name_of = |m: &mut M| m.engine_ref().p.as_ref().map(|p| p.registry.entries[id.0 as usize].clone_name()).unwrap_or_default();
    if calls > max_calls {
        return stop(m, format!("API call budget exhausted ({max_calls})"));
    }
    let ra = m.read_u32(regs.esp).unwrap_or(0);
    if trace {
        let name = name_of(m);
        let args = format_args(m, regs.esp, if nargs > 0 { nargs.min(8) } else { 4 });
        let line = format!("{:w$}{name}({args}) from {ra:#x}", "", w = (depth * 2) as usize);
        m.print(&line);
    }
    let Some(f) = func else {
        let name = name_of(m);
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
                regs.esp += 4 + pop.unwrap_or(ret.pop);
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
        Err(ApiError(msg)) => {
            let name = name_of(m);
            stop(m, format!("{name}: {msg}"))
        }
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
    let form = iced_x86::Code::try_from(code as usize).unwrap_or(iced_x86::Code::INVALID);
    let mn = form.mnemonic();
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
        *h.entry((form, ip)).or_default() += 1;
    }
    let r = f.exec(mn, a, b, ip, fop as u16, &mut FpuMem(m));
    fpu_store(m, &f);
    r
}

pub fn cpuid_regs(leaf: u32, _sub: u32) -> (u32, u32, u32, u32) {
    match leaf {
        0 => (1, 0x756E_6547, 0x6C65_746E, 0x4965_6E69),
        // FPU, TSC, CX8, CMOV, FXSR, SSE, SSE2. MMX is translated (code that only checks
        // SSE uses it) but not advertised, so code that asks takes the paths tested so far.
        1 => (0x0001_06A5, 0x0001_0800, 0, 0x0700_8111),
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
        let mut functions = HashSet::default();
        let mut jump_tables = HashMap::default();
        let mut code_id = 0x811c_9dc5u32;
        match pe::Pe::parse(&file) {
            Ok(pe) => {
                for sec in pe.sections.iter().filter(|s| s.is_executable()) {
                    let (a, n) = (sec.raw_offset as usize, sec.raw_size as usize);
                    for &b in file.get(a..a.saturating_add(n).min(file.len())).unwrap_or(&[]) {
                        code_id = (code_id ^ b as u32).wrapping_mul(0x0100_0193);
                    }
                }
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
        let mut sorted: Vec<u32> = functions.iter().copied().collect();
        sorted.sort_unstable();
        m.engine().mods.push(ModInfo { name, base, size, functions, sorted, jump_tables, code_id });
    }
}

/// Functions a module holds at most when it is translated for a call: the called
/// function, those it calls directly, then its neighbours in the image (code used
/// together tends to lie together). Browsers bound the number of modules (Firefox
/// runs out of code memory at a few thousand), so one per function does not last a
/// long game.
pub const CALL_BATCH: usize = 16;

/// Translates the functions starting at `entries` into one module and registers them,
/// together with untranslated functions they call directly, then functions next to
/// the first one, until the module holds `fill`. Returns the entries that could not
/// be translated.
pub fn translate_batch<M: Machine>(m: &mut M, entries: &[u32], fill: usize) -> Result<Vec<u32>, String> {
    translate_batch_checked(m, entries, fill, &[]).map(|(failed, _)| failed)
}

/// `translate_batch`, leaving out the entries of `expect` ([address, code hash]) whose
/// code hashes otherwise now (code an earlier session saw, since changed). Returns the
/// entries that failed and those left out.
pub fn translate_batch_checked<M: Machine>(m: &mut M, entries: &[u32], fill: usize, expect: &[(u32, u32)]) -> Result<(Vec<u32>, Vec<u32>), String> {
    let t = m.now_us();
    let mut mismatched = Vec::new();
    let shared = m.engine_ref().shared_memory;
    let funcs = std::mem::take(&mut m.engine().funcs);
    let mods = std::mem::take(&mut m.engine().mods);
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    let cache = m.engine().inline_cache.take().unwrap_or_default();
    let out = {
        let mem = CodeMem(&*m);
        let probes = m.engine_ref().probes.clone();
        let no_x87_inline = m.engine_ref().no_x87_inline;
        let count = m.engine_ref().count;
        let count_blocks = m.engine_ref().count_blocks;
        let no_inline = m.engine_ref().no_inline;
        // calls to translated functions use their table index (watched ones report)
        let watch = m.engine_ref().watch.clone();
        let known = |a: u32| if watch.contains(&a) { None } else { funcs.get(&a).copied() };
        let mut tr = recomp::Translator::new(&mem, recomp::Options { shared_memory: shared, probes, no_x87_inline, resolve_cache: true, count, count_blocks, no_inline }).with_cache(cache);
        for &entry in entries {
            let md = mods.iter().find(|md| entry >= md.base && entry < md.base + md.size);
            let empty_jt = std::collections::HashMap::new();
            let jt = md.map(|md| &md.jump_tables).unwrap_or(&empty_jt);
            let is_fn = |a: u32| funcs.contains_key(&a) || md.map(|md| md.functions.contains(&a)).unwrap_or(false);
            let info = FuncInfo { jump_tables: jt, is_function: &is_fn, translated: &known };
            if let Err(e) = tr.add_function(entry, &info) {
                errors.push(format!("translate {entry:#x}: {e}"));
                failed.push(entry);
            }
        }
        for &(a, h) in expect {
            if tr.entries().any(|e| e == a) && tr.code_hash(a) != Some(h) {
                tr.remove_function(a);
                mismatched.push(a);
            }
        }
        // callees, breadth first; one that fails is left for when it is called
        let bad = &m.engine_ref().bad_functions;
        let mut queue: std::collections::VecDeque<u32> = entries.iter().flat_map(|&e| tr.direct_targets(e)).collect();
        let mut tried: HashSet<u32> = HashSet::default();
        while tr.len() < fill {
            let Some(a) = queue.pop_front() else { break };
            if !tried.insert(a) || funcs.contains_key(&a) || bad.contains(&a) || Process::stub_id(a).is_some() {
                continue;
            }
            let Some(md) = mods.iter().find(|md| a >= md.base && a < md.base + md.size) else { continue };
            let is_fn = |x: u32| funcs.contains_key(&x) || md.functions.contains(&x);
            let info = FuncInfo { jump_tables: &md.jump_tables, is_function: &is_fn, translated: &known };
            if tr.add_function(a, &info).is_ok() {
                queue.extend(tr.direct_targets(a));
            }
        }
        // then neighbours, unless ahead translation runs (the host waits for real
        // time): it gets to them, and the call that is waiting does not pay for them
        let neighbours = m.engine_ref().idle_calls == 0;
        if let Some(md) = entries.first().and_then(|&e| mods.iter().find(|md| e >= md.base && e < md.base + md.size)).filter(|_| tr.len() < fill && neighbours) {
            let at = md.sorted.partition_point(|&a| a < entries[0]);
            let after = md.sorted[at..].iter();
            let before = md.sorted[..at].iter().rev();
            // the next ones first; a bounded look either way
            for &a in after.take(256).chain(before.take(64)) {
                if tr.len() >= fill {
                    break;
                }
                if !tried.insert(a) || funcs.contains_key(&a) || bad.contains(&a) {
                    continue;
                }
                let is_fn = |x: u32| funcs.contains_key(&x) || md.functions.contains(&x);
                let info = FuncInfo { jump_tables: &md.jump_tables, is_function: &is_fn, translated: &known };
                let _ = tr.add_function(a, &info);
            }
        }
        let cache = tr.take_cache();
        let frontier: Vec<u32> = tr.entries().flat_map(|e| tr.direct_targets(e)).filter(|t| !funcs.contains_key(t)).collect();
        let hashes: Vec<(u32, u32)> = tr.entries().map(|e| (e, tr.code_hash(e).unwrap_or(0))).collect();
        (tr.finish(), cache, frontier, hashes)
    };
    let (out, cache, frontier, hashes) = out;
    m.engine().inline_cache = Some(cache);
    m.engine().idle_next.extend(frontier);
    m.engine().funcs = funcs;
    m.engine().mods = mods;
    for e in errors {
        m.print(&e);
    }
    if out.functions.is_empty() {
        return Ok((failed, mismatched));
    }
    let names: Vec<String> = out.functions.iter().map(|(_, n)| n.clone()).collect();
    // GMR_DUMP_FN=hex: write the module that holds this function (debugging)
    if let Some(a) = std::env::var("GMR_DUMP_FN").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        if out.functions.iter().any(|(e, _)| *e == a) {
            let _ = std::fs::write(format!("target/fn-{a:x}.wasm"), &out.wasm);
        }
    }
    let indices = match m.instantiate(&out.wasm, &names) {
        Ok(ix) => ix,
        Err(e) => {
            if entries.len() == 1 {
                return Err(format!("wasm for function {:#x} rejected: {e}", entries[0]));
            }
            // Bisect to isolate the offending function.
            let (a, b) = entries.split_at(entries.len() / 2);
            failed.extend(translate_batch(m, a, a.len())?);
            failed.extend(translate_batch(m, b, b.len())?);
            return Ok((failed, mismatched));
        }
    };
    let now = m.now_us();
    let e = m.engine();
    for ((entry, _), idx) in out.functions.iter().zip(indices) {
        e.funcs.insert(*entry, idx);
    }
    e.last_batch = hashes;
    e.translated += out.functions.len();
    e.modules += 1;
    e.translate_us += now - t;
    let trace = e.trace;
    if std::env::var_os("GMR_BATCH_LOG").is_some() {
        let first = entries.first().copied().unwrap_or(0);
        let known = e.mods.iter().any(|md| md.functions.contains(&first));
        let in_mod = e.mods.iter().find(|md| first >= md.base && first < md.base + md.size).map(|md| md.name.clone()).unwrap_or_default();
        let n = out.functions.len();
        m.print(&format!("batch {first:#x} [{in_mod}{}] {n} functions", if known { "" } else { ", not discovered" }));
    }
    if trace {
        let lines: Vec<String> = out.unimplemented.iter().map(|(a, t)| format!("unimplemented instruction at {a:#x}: {t}")).collect();
        for l in lines {
            m.print(&l);
        }
    }
    Ok((failed, mismatched))
}

/// Translates functions discovery found before anything calls them, in batches, until
/// host time `deadline_us`. The browser does this while it waits for real time, so
/// code a game reaches later does not stall the frame it first runs in; the guest
/// sees no difference. Returns how many were translated.
pub fn translate_idle<M: Machine>(m: &mut M, deadline_us: u64) -> usize {
    if m.engine_ref().idle_queue.is_none() {
        // (address, bytes up to the next function or the module's end)
        let mut q: Vec<(u32, u32)> = Vec::new();
        for md in &m.engine_ref().mods {
            for (k, &a) in md.sorted.iter().enumerate() {
                let end = md.sorted.get(k + 1).copied().unwrap_or(md.base + md.size);
                q.push((a, end.saturating_sub(a).clamp(1, 0x10000)));
            }
        }
        q.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let e = m.engine();
        e.idle_queue = Some(q);
        if e.idle_ns_per_byte == 0 {
            e.idle_ns_per_byte = 600;
        }
    }
    let mut done = 0;
    // The first wait after the first frame (packers have unpacked; a loading screen is
    // up): the profile at once, for up to PROFILE_BURST_US, so the frames that need it
    // later find it ready.
    let e = m.engine();
    if e.frames_seen > 0 && !e.profile_burst && !e.profile.is_empty() {
        e.profile_burst = true;
        let t = m.now_us();
        let end = t + PROFILE_BURST_US;
        let mut skipped = [0; 3];
        while m.now_us() < end {
            let batch = profile_batch(m, 16, PROFILE_BURST_FRAMES, &mut skipped);
            if batch.is_empty() {
                break;
            }
            let addrs: Vec<u32> = batch.iter().map(|&(a, _)| a).collect();
            if let Ok((_, changed)) = translate_batch_checked(m, &addrs, addrs.len(), &batch) {
                skipped[2] += changed.len();
                done += addrs.len() - changed.len();
            }
        }
        let left = m.engine_ref().profile.len();
        let ms = (m.now_us() - t) / 1000;
        m.print(&format!(
            "translation profile: {done} functions translated in {ms} ms ({} translated already, {} not mapped, {} changed, {left} left)",
            skipped[0], skipped[1], skipped[2]
        ));
        return done;
    }
    loop {
        // what fits in the time left by the running estimate (half again: a slow
        // compile can exceed it), in modules of 4 to 16 functions (browsers bound how
        // many modules there are); a function too large for the time a wait leaves is
        // left to be translated when it runs
        // (estimates with a quarter in reserve: a slow compile can exceed them)
        let budget_ns = (deadline_us.saturating_sub(m.now_us()) * 1000).saturating_sub(m.engine_ref().idle_fixed_ns * 5 / 4);
        let per = m.engine_ref().idle_ns_per_byte.max(1) * 5 / 4;
        let mut expect = Vec::new();
        let (batch, bytes) = {
            let e = m.engine();
            let q = e.idle_queue.as_mut().expect("queue built above");
            let (mut b, mut bytes) = (Vec::new(), 0u64);
            // what earlier sessions needed, then the call graph's frontier, sized like
            // the queue's entries
            let started = e.frames_seen > 0;
            while started && b.len() < 16 {
                let Some(&(a, h, _)) = e.profile.last() else { break };
                let Some(md) = e.mods.iter().find(|md| a >= md.base && a < md.base + md.size) else {
                    e.profile.pop();
                    continue;
                };
                if e.funcs.contains_key(&a) || e.bad_functions.contains(&a) || b.iter().any(|&(x, _)| x == a) {
                    e.profile.pop();
                    continue;
                }
                let k = md.sorted.partition_point(|&f| f <= a);
                let size = md.sorted.get(k).copied().unwrap_or(md.base + md.size).saturating_sub(a).clamp(1, 0x10000);
                if size as u64 * per > budget_ns.max(4_000_000) {
                    e.profile.pop(); // too large to translate ahead
                    continue;
                }
                if (bytes + size as u64) * per > budget_ns {
                    break;
                }
                e.profile.pop();
                b.push((a, size));
                expect.push((a, h));
                bytes += size as u64;
            }
            while let Some(a) = e.idle_next.pop() {
                if e.funcs.contains_key(&a) || e.bad_functions.contains(&a) || b.iter().any(|&(x, _)| x == a) {
                    continue;
                }
                let Some(md) = e.mods.iter().find(|md| a >= md.base && a < md.base + md.size) else { continue };
                let k = md.sorted.partition_point(|&f| f <= a);
                let size = md.sorted.get(k).copied().unwrap_or(md.base + md.size).saturating_sub(a).clamp(1, 0x10000);
                if (bytes + size as u64) * per > budget_ns || b.len() >= 16 {
                    e.idle_next.push(a);
                    break;
                }
                b.push((a, size));
                bytes += size as u64;
            }
            while b.len() < 16 {
                let Some(&(a, size)) = q.last() else { break };
                if e.funcs.contains_key(&a) || e.bad_functions.contains(&a) {
                    q.pop();
                    continue;
                }
                if size as u64 * per > budget_ns.max(4_000_000) {
                    q.pop(); // too large to translate ahead
                    continue;
                }
                if (bytes + size as u64) * per > budget_ns {
                    break;
                }
                q.pop();
                b.push((a, size));
                bytes += size as u64;
            }
            (b, bytes)
        };
        let last = m.engine_ref().idle_queue.as_ref().is_some_and(|q| q.is_empty());
        if batch.is_empty() || batch.len() < 4 && !last {
            // too few for a module: back on the queue for a longer wait
            let q = m.engine().idle_queue.as_mut().unwrap();
            q.extend(batch.into_iter().rev());
            break;
        }
        let batch: Vec<u32> = batch.into_iter().map(|(a, _)| a).collect();
        // Failures are not recorded: a function can fail now and translate when it
        // runs (code a packer has not unpacked yet).
        let t = m.now_us();
        let _ = translate_batch_checked(m, &batch, batch.len(), &expect);
        // keep the estimate current (it includes the host's compile and instantiate):
        // up halfway (one slow compile should not stop it for good), down slowly
        let took = (m.now_us() - t) * 1000;
        let e = m.engine();
        e.idle_samples.push_back((bytes, took));
        if e.idle_samples.len() > 32 {
            e.idle_samples.pop_front();
        }
        fit_idle_cost(e);
        done += batch.len();
    }
    let e = m.engine();
    e.idle_calls += 1;
    e.idle_done += done;
    done
}

/// Fits `idle_fixed_ns` + `idle_ns_per_byte` × bytes to the recent idle batches
/// (least squares), keeping both in sane bounds; with too few samples or sizes too
/// alike for a slope, the rate follows the batches' average with the fixed cost.
fn fit_idle_cost(e: &mut Engine) {
    let n = e.idle_samples.len() as f64;
    if n < 1.0 {
        return;
    }
    let (sb, st) = e.idle_samples.iter().fold((0.0, 0.0), |(b, t), &(x, y)| (b + x as f64, t + y as f64));
    let (mb, mt) = (sb / n, st / n);
    let (mut cov, mut var) = (0.0, 0.0);
    for &(x, y) in &e.idle_samples {
        cov += (x as f64 - mb) * (y as f64 - mt);
        var += (x as f64 - mb) * (x as f64 - mb);
    }
    let (fixed, per) = if n >= 6.0 && var > 0.0 && cov > 0.0 {
        let per = cov / var;
        (mt - per * mb, per)
    } else {
        let fixed = e.idle_fixed_ns as f64;
        (fixed, (mt - fixed).max(0.0) / mb.max(1.0))
    };
    e.idle_ns_per_byte = (per as u64).clamp(100, 20_000);
    e.idle_fixed_ns = (fixed.max(0.0) as u64).clamp(100_000, 3_000_000);
}

/// `translate_idle`, with its host time counted (`Engine::idle_us`).
pub fn translate_idle_timed<M: Machine>(m: &mut M, deadline_us: u64) -> usize {
    let t = m.now_us();
    let n = translate_idle(m, deadline_us);
    let dt = m.now_us() - t;
    m.engine().idle_us += dt;
    n
}

/// Translates every function discovery found in the loaded modules.
pub fn translate_all<M: Machine>(m: &mut M, chunk: usize) -> Result<usize, String> {
    sync_modules(m);
    let mut entries: Vec<u32> = m.engine_ref().mods.iter().flat_map(|md| md.functions.iter().copied()).filter(|a| !m.engine_ref().funcs.contains_key(a)).collect();
    entries.sort_unstable();
    let mut failed = 0;
    for c in entries.chunks(chunk.max(1)) {
        failed += translate_batch(m, c, c.len())?.len();
    }
    Ok(failed)
}

pub fn resolve_addr<M: Machine>(m: &mut M, addr: u32) -> Result<u32, String> {
    // (API stubs first: most calls that get here are API calls)
    if let Some(id) = Process::stub_id(addr) {
        call_api(m, id);
        return Ok(m.engine_ref().nop_index);
    }
    if let Some(&i) = m.engine_ref().funcs.get(&addr) {
        m.engine().resolves += 1;
        cache_resolved(m, addr, i);
        return Ok(i);
    }
    if (RET_TO_HOST..RET_TO_HOST + 0x1000).contains(&addr) {
        return Err(format!("guest returned to a host frame marker {addr:#x} that is not the innermost one"));
    }
    if m.engine_ref().bad_functions.contains(&addr) {
        return Err(format!("function {addr:#x} cannot be translated"));
    }
    sync_modules(m);
    // Not a function start: most likely a return address the guest goes back to after
    // its stack was unwound (a thread switch). The frames above it return to theirs
    // next, so those go into the same module.
    let mut entries = vec![addr];
    if !m.engine_ref().mods.iter().any(|md| md.functions.contains(&addr)) {
        entries.extend(stack_continuations(m, addr, CALL_BATCH - 1));
    }
    let failed = translate_batch(m, &entries, CALL_BATCH)?;
    if failed.contains(&addr) {
        m.engine().bad_functions.insert(addr);
        return Err(format!("cannot translate function at {addr:#x}"));
    }
    let i = m.engine_ref().funcs.get(&addr).copied().ok_or_else(|| format!("translation of {addr:#x} produced no function"))?;
    cache_resolved(m, addr, i);
    // for the next session's profile: the whole batch (what this function calls came
    // with it)
    let e = m.engine();
    for (a, h) in std::mem::take(&mut e.last_batch) {
        if h != 0 && e.profile_known.insert(a) {
            e.demand.push((a, h, e.frames_seen as u32));
        }
    }
    Ok(i)
}

/// How long the profile's burst may hold the first frame up.
const PROFILE_BURST_US: u64 = 2_000_000;
/// The burst takes what was needed in the first this many frames; later entries are
/// translated while the game waits (`translate_idle` takes them before anything else).
const PROFILE_BURST_FRAMES: u32 = 300;

/// Before any guest code runs, the profile entries needed before the first frame are
/// translated, in the order they were needed, when everything they were translated
/// from (their instructions, inlined callees' too) is what the profile saw: code a
/// packer has yet to unpack, or startup code will patch, differs now and waits for
/// the burst after the first frame. Startup then translates in full batches instead
/// of a call at a time.
fn profile_prefetch<M: Machine>(m: &mut M, budget_us: u64) {
    if m.engine_ref().profile.is_empty() {
        return;
    }
    sync_modules(m);
    let t = m.now_us();
    // (the profile is a stack, first needed last); what was needed before the first
    // frame, and matches
    let entries: Vec<(u32, u32, u32)> = m.engine().profile.drain(..).rev().collect();
    let (mut now, mut later) = (Vec::new(), Vec::new());
    for (a, h, frame) in entries {
        let e = m.engine_ref();
        let mapped = e.mods.iter().any(|md| a >= md.base && a < md.base + md.size);
        if frame == 0 && mapped {
            now.push((a, h));
        } else {
            later.push((a, h, frame));
        }
    }
    // (those whose code differs now, and any the time runs out for, wait for later)
    let mut done = 0;
    for chunk in now.chunks(16) {
        let batch: Vec<(u32, u32)> = chunk.iter().copied().filter(|(a, _)| !m.engine_ref().funcs.contains_key(a)).collect();
        if batch.is_empty() {
            continue;
        }
        if m.now_us() - t > budget_us {
            later.extend(batch.iter().map(|&(a, h)| (a, h, 0)));
            continue;
        }
        let addrs: Vec<u32> = batch.iter().map(|&(a, _)| a).collect();
        match translate_batch_checked(m, &addrs, addrs.len(), &batch) {
            Ok((_, changed)) => {
                done += addrs.len() - changed.len();
                later.extend(batch.iter().filter(|(a, _)| changed.contains(a)).map(|&(a, h)| (a, h, 0)));
            }
            Err(_) => later.extend(batch.iter().map(|&(a, h)| (a, h, 0))),
        }
    }
    // back to a stack in order of need
    later.sort_by_key(|e| std::cmp::Reverse(e.2));
    m.engine().profile = later;
    let ms = (m.now_us() - t) / 1000;
    let left = m.engine_ref().profile.len();
    m.print(&format!("translation profile: {done} functions translated before the start in {ms} ms ({left} left for after the first frame)"));
}

/// Up to `max` profile entries needed by frame `frames` that are mapped and not
/// translated yet (others are dropped), taken off the profile; their code is checked
/// as they are translated (`translate_batch_checked`).
fn profile_batch<M: Machine>(m: &mut M, max: usize, frames: u32, skipped: &mut [usize; 3]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    while out.len() < max {
        if m.engine_ref().profile.last().is_none_or(|e| e.2 > frames) {
            break;
        }
        let Some((a, h, _)) = m.engine().profile.pop() else { break };
        let e = m.engine_ref();
        if e.funcs.contains_key(&a) || e.bad_functions.contains(&a) || out.iter().any(|&(x, _)| x == a) {
            skipped[0] += 1;
        } else if !e.mods.iter().any(|md| a >= md.base && a < md.base + md.size) {
            skipped[1] += 1;
        } else {
            out.push((a, h));
        }
    }
    out
}

/// Queues a profile entry: they go in last needed first (`profile` is a stack).
pub fn profile_push<M: Machine>(m: &mut M, addr: u32, hash: u32, frame: u32) {
    let e = m.engine();
    if e.profile_known.insert(addr) {
        e.profile.push((addr, hash, frame));
    }
}

/// Return addresses on the guest stack (values just after a call instruction in a
/// loaded module) that have no translation yet, nearest first.
fn stack_continuations<M: Machine>(m: &mut M, addr: u32, max: usize) -> Vec<u32> {
    let esp = m.global(Global::Esp);
    let mut out = Vec::new();
    for k in 0..256 {
        if out.len() >= max {
            break;
        }
        // (the stack's top ends the scan)
        let Ok(v) = m.read_u32(esp.wrapping_add(4 * k)) else { break };
        let e = m.engine_ref();
        if v == addr || out.contains(&v) || e.funcs.contains_key(&v) || e.bad_functions.contains(&v) || !e.mods.iter().any(|md| v >= md.base + 7 && v < md.base + md.size) {
            continue;
        }
        let mut b = [0u8; 7];
        if m.read(v - 7, &mut b).is_err() {
            continue;
        }
        // call rel32, or call r/m32 (FF /2) of 2, 3, 4, 6 or 7 bytes
        let after_call = b[2] == 0xE8 || [5usize, 4, 3, 1, 0].iter().any(|&k| b[k] == 0xFF && (b[k + 1] >> 3) & 7 == 2);
        if after_call {
            out.push(v);
        }
    }
    out
}

/// Enters a translated function in the resolve cache that translated code probes
/// (`abi::RESOLVE_CACHE`). Watched addresses stay out, so every call reports.
fn cache_resolved<M: Machine>(m: &mut M, addr: u32, idx: u32) {
    if m.engine_ref().watch.contains(&addr) {
        return;
    }
    let mut entry = [0u8; 8];
    entry[..4].copy_from_slice(&addr.to_le_bytes());
    entry[4..].copy_from_slice(&idx.to_le_bytes());
    let _ = m.write(abi::RESOLVE_CACHE + abi::resolve_slot(addr), &entry);
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
                Some(Ctl::Resume) | Some(Ctl::Yield) | None => {}
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

// ---- guest threads ----------------------------------------------------------------------

fn save_ctx<M: Machine + ?Sized>(m: &mut M) -> ThreadCtx {
    let flags = [Global::Cf, Global::Pf, Global::Af, Global::Zf, Global::Sf, Global::Of, Global::Df].map(|g| m.global(g));
    let mut xmm = [0u128; 8];
    for (i, x) in xmm.iter_mut().enumerate() {
        *x = m.xmm(i as u32);
    }
    ThreadCtx { regs: read_regs(m), flags, xmm, mxcsr: m.global(Global::Mxcsr) }
}

fn restore_ctx<M: Machine + ?Sized>(m: &mut M, c: &ThreadCtx) {
    write_regs(m, &c.regs);
    for (g, v) in [Global::Cf, Global::Pf, Global::Af, Global::Zf, Global::Sf, Global::Of, Global::Df].into_iter().zip(c.flags) {
        m.set_global(g, v);
    }
    for (i, x) in c.xmm.iter().enumerate() {
        m.set_xmm(i as u32, *x);
    }
    m.set_global(Global::Mxcsr, c.mxcsr);
}

enum SliceEnd {
    Yielded,
    Preempted,
    Exited(u32),
}

/// Runs the thread whose registers are in the globals until it blocks, ends or
/// uses up its budget.
fn run_slice<M: Machine>(m: &mut M) -> Result<SliceEnd, String> {
    let start = m.engine_ref().dispatches;
    let mut eip = m.global(Global::Eip);
    loop {
        if eip == crate::process::THREAD_EXIT {
            return Ok(SliceEnd::Exited(m.global(Global::Eax)));
        }
        if m.engine_ref().dispatches - start > SLICE_BUDGET {
            return Ok(SliceEnd::Preempted);
        }
        m.engine().dispatches += 1;
        let idx = resolve_addr(m, eip)?;
        m.call_index(idx)?;
        if m.global(Global::Ctl) != 0 {
            m.set_global(Global::Ctl, 0);
            match m.engine().ctl.take() {
                Some(Ctl::Stop(msg)) => {
                    m.engine().ctl = Some(Ctl::Stop(msg.clone()));
                    return Err(msg);
                }
                Some(Ctl::Fault { code, ip }) => raise_fault(m, code, ip)?,
                Some(Ctl::Yield) => return Ok(SliceEnd::Yielded),
                Some(Ctl::Resume) | None => {}
            }
        }
        eip = m.global(Global::Eip);
    }
}

/// Runs every secondary thread that can run now (from the main thread's blocking
/// points), each until it blocks again. The main thread's registers are kept.
/// Whether `run_threads` has a thread to run.
fn threads_ready(p: &mut Process) -> bool {
    p.current == 0 && !p.in_scheduler && (1..p.threads.len()).any(|i| crate::hle::threads::ready(p, i, false))
}

pub fn run_threads<M: Machine>(m: &mut M, p: &mut Process) {
    if !threads_ready(p) {
        return;
    }
    p.in_scheduler = true;
    let main = save_ctx(m);
    for i in 1..p.threads.len() {
        if !crate::hle::threads::ready(p, i, true) {
            continue;
        }
        let Some(ctx) = p.threads[i].ctx.take() else { continue };
        restore_ctx(m, &ctx);
        if let Some(eax) = p.threads[i].resume_eax.take() {
            m.set_global(Global::Eax, eax);
        }
        p.threads[i].state = crate::process::ThreadState::Running;
        p.current = i;
        p.slice_nest = Some(p.nest);
        let owned = std::mem::replace(p, Process::empty());
        m.engine().p = Some(Box::new(owned));
        let r = run_slice(m);
        if let Some(back) = m.engine().p.take() {
            *p = *back;
        }
        p.current = 0;
        p.slice_nest = None;
        let t = &mut p.threads[i];
        match r {
            Ok(SliceEnd::Exited(code)) => {
                t.state = crate::process::ThreadState::Exited(code);
            }
            Ok(end) => {
                if matches!(end, SliceEnd::Preempted) || t.state == crate::process::ThreadState::Running {
                    t.state = crate::process::ThreadState::Ready;
                }
                t.ctx = Some(Box::new(save_ctx(m)));
            }
            Err(e) => {
                // A thread that fails ends; the process goes on (and says so).
                let id = t.id;
                t.state = crate::process::ThreadState::Exited(u32::MAX);
                m.engine().ctl = None;
                p.log(format!("thread {id:#x} stopped: {e}"));
                m.print(&format!("thread {id:#x} stopped: {e}"));
            }
        }
    }
    restore_ctx(m, &main);
    p.in_scheduler = false;
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
    let mut seen = HashSet::default();
    // Imported DLLs come from the game directory, else the system directory.
    let read = |m: &M, name: &str| {
        let fs = &m.engine_ref().process().fs;
        fs.read_all(name).or_else(|| if name.contains('\\') { None } else { fs.read_all(&format!("{}\\{name}", crate::hle::kernel32::SYSTEM_DIR)) })
    };
    {
        let r = |name: &str| read(m, name);
        collect_order(exe, &r, &mut seen, &mut order)?;
    }
    // Windows maps the exe before its DLLs: keep its preferred range free for it (an
    // exe without relocations cannot go anywhere else).
    let exe_range = read(m, exe).and_then(|b| pe::Pe::parse(&b).ok().map(|pe| (pe.image_base, pe.size_of_image.div_ceil(0x1000) * 0x1000)));
    let mut reserved = exe_range.and_then(|(base, size)| m.engine().p.as_mut()?.vm.alloc(base, size, false, crate::mem::PAGE_NOACCESS, false, "exe"));
    let mut attach = Vec::new();
    let (mut entry, mut image_base) = (0, 0);
    for name in order {
        let bytes = read(m, &name).ok_or_else(|| format!("reading {name}"))?;
        let in_game = m.engine_ref().process().fs.is_file(&name);
        let guest_path = if in_game { format!("C:\\game\\{name}") } else { format!("C:\\Windows\\system32\\{name}") };
        let mut p = m.engine().p.take().ok_or("process not available")?;
        if name.eq_ignore_ascii_case(exe) {
            if let Some(base) = reserved.take() {
                p.vm.release(base);
            }
        }
        let loaded = {
            let req = crate::LoadRequest { name: &name, path: &guest_path, bytes };
            let mut resolver = crate::StubResolver { registry: &mut p.registry, modules: &p.modules };
            let r = crate::loader::load(req, &mut p.vm, m, &mut resolver);
            p.registry.flush_data(m);
            r
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
    m.ensure_mapped(abi::RESOLVE_CACHE, abi::RESOLVE_CACHE_SIZE + 0x1000 + 8 * abi::ENTRY_SLOTS).map_err(|_| "cannot map the resolve cache".to_string())?;
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
    profile_prefetch(m, PROFILE_BURST_US);
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
    use wasm_encoder::{CodeSection, ExportKind, ExportSection, Function, FunctionSection, Module, TypeSection, ValType};
    // a translated function's type: it returns the registers it gets
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32; 8], [ValType::I32; 8]);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    let mut exports = ExportSection::new();
    exports.export("nop", ExportKind::Func, 0);
    let mut code = CodeSection::new();
    let mut f = Function::new([]);
    for r in 0..8 {
        f.instruction(&wasm_encoder::Instruction::LocalGet(r));
    }
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
    // (not eip: when a run stops inside translated code it holds the last `ret`'s
    // target, which depends on what was inlined)
    for r in [regs.eax, regs.ecx, regs.edx, regs.ebx, regs.esp, regs.ebp, regs.esi, regs.edi] {
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
        // The resolve cache depends on how code was translated (batches, order).
        let cache = abi::RESOLVE_CACHE as u64..abi::RESOLVE_CACHE as u64 + abi::RESOLVE_CACHE_SIZE as u64;
        if !cache.contains(&addr) && buf.iter().any(|&b| b != 0) {
            mix(&mut h, &((addr / 65536) as u32).to_le_bytes());
            mix(&mut h, &buf);
            chunks += 1;
        }
        addr += 65536;
    }
    (h, chunks)
}
