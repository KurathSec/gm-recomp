//! High-level emulation of Win32 APIs. Each API is a Rust function that reads its
//! stdcall arguments from the guest stack and returns the value for EAX plus how
//! many argument bytes the callee pops.

pub mod audio;
pub mod com;
pub mod d3d8;
pub mod d3d9;
pub mod d3d9_shader;
pub mod dmusic;
pub mod dsound;
pub mod gpu;
pub mod gdi32;
pub mod kernel32;
pub mod mci;
pub mod misc;
pub mod msvcrt;
pub mod nls;
pub mod user32;

use crate::mem::Guest;
use crate::process::Process;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiId(pub u32);

#[derive(Debug, Clone, Copy, Default)]
pub struct Regs {
    pub eax: u32,
    pub ecx: u32,
    pub edx: u32,
    pub ebx: u32,
    pub esp: u32,
    pub ebp: u32,
    pub esi: u32,
    pub edi: u32,
    pub eip: u32,
    pub fs_base: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Ret {
    pub eax: u32,
    /// Bytes of arguments to pop (stdcall); 0 for cdecl.
    pub pop: u32,
    /// The API rewrote `Ctx::regs` wholesale (including eip and esp) and execution
    /// continues there instead of returning to the caller: exception dispatch,
    /// `NtContinue`, thread switches.
    pub resume: bool,
    /// High half of a 64-bit result.
    pub edx: Option<u32>,
}

impl Ret {
    pub fn stdcall(nargs: u32, eax: u32) -> Self {
        Ret { eax, pop: nargs * 4, resume: false, edx: None }
    }
    pub fn cdecl(eax: u32) -> Self {
        Ret { eax, pop: 0, resume: false, edx: None }
    }
    pub fn resume() -> Self {
        Ret { eax: 0, pop: 0, resume: true, edx: None }
    }
}

/// Error raised by an API: ends the run with a message.
#[derive(Debug)]
pub struct ApiError(pub String);

pub type ApiResult = Result<Ret, ApiError>;

/// Services the host provides to APIs beyond raw memory: calling back into guest code.
pub trait Host: Guest {
    /// Calls guest `addr` with stdcall `args`; returns EAX.
    fn call_guest(&mut self, p: &mut Process, regs: &mut Regs, addr: u32, args: &[u32]) -> Result<u32, String>;
    /// Ends execution of the guest (ExitProcess).
    fn request_exit(&mut self, code: u32);
    /// Pushes a value onto the x87 stack (FLD m64 semantics).
    fn fpu_push(&mut self, v: f64);
    /// Pops ST(0) (FSTP m64 semantics).
    fn fpu_pop(&mut self) -> f64;
    /// The calling thread's x87 state, and writing it back (control/status words).
    fn fpu_state(&mut self) -> fpu::Fpu;
    fn set_fpu_state(&mut self, f: &fpu::Fpu);
    /// SSE control/status register.
    fn mxcsr(&mut self) -> u32;
    fn set_mxcsr(&mut self, v: u32);
    /// Input that arrived since the last call (keyboard, mouse); `frame` is the number
    /// of frames presented so far.
    fn poll_input(&mut self, _frame: u64) -> Vec<InputEvent> {
        Vec::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    /// Virtual-key code, scan code (with 0x100 for extended keys), pressed.
    Key { vk: u8, scan: u16, down: bool },
    /// Client coordinates of the game window and the button state (bit 0 left,
    /// bit 1 right, bit 2 middle).
    Mouse { x: i32, y: i32, buttons: u32 },
}

pub struct Ctx<'a> {
    pub host: &'a mut dyn Host,
    pub regs: &'a mut Regs,
    pub p: &'a mut Process,
    pub id: ApiId,
}

impl Ctx<'_> {
    /// Stdcall argument `i` (0-based) from the guest stack.
    pub fn arg(&self, i: u32) -> u32 {
        self.host.read_u32(self.regs.esp + 4 + 4 * i).unwrap_or(0)
    }
    pub fn arg_i32(&self, i: u32) -> i32 {
        self.arg(i) as i32
    }
    pub fn str_arg(&self, i: u32) -> String {
        let a = self.arg(i);
        if a == 0 { String::new() } else { self.host.read_cstr_lossy(a) }
    }
    pub fn wstr_arg(&self, i: u32) -> String {
        let a = self.arg(i);
        if a == 0 { String::new() } else { self.host.read_wstr(a).unwrap_or_default() }
    }
    pub fn set_last_error(&mut self, e: u32) {
        let teb = self.regs.fs_base;
        let _ = self.host.write_u32(teb + 0x34, e);
        self.p.current_thread_mut().last_error = e;
    }
    pub fn name(&self) -> String {
        self.p.registry.name(self.id)
    }
    pub fn trace(&mut self, detail: String) {
        if self.p.trace {
            let n = self.name();
            self.p.log(format!("  {n}{detail}"));
        }
    }
    pub fn call_guest(&mut self, addr: u32, args: &[u32]) -> Result<u32, ApiError> {
        self.host.call_guest(self.p, self.regs, addr, args).map_err(ApiError)
    }
    /// Advances the virtual clock (sleeps, idle waits) and everything driven by it.
    pub fn advance_clock(&mut self, us: u64) {
        self.p.clock_us += us;
        dsound::tick(self);
    }
    /// A clock read. Back-to-back reads with no other work in between (a spin-wait on
    /// the time) advance the clock in doubling steps of 1..128 µs, so polling loops
    /// end deterministically without skipping far past their deadline.
    pub fn poll_clock(&mut self) {
        let step = 1u64 << self.p.idle_streak.min(7);
        self.p.idle_streak += 1;
        self.advance_clock(step);
    }
    /// Declared argument count of the current API (for tables of uniform methods).
    pub fn nargs(&self) -> u32 {
        self.p.registry.entries.get(self.id.0 as usize).map(|e| e.nargs).unwrap_or(0)
    }
}

pub type ApiFn = fn(&mut Ctx) -> ApiResult;

pub struct ApiEntry {
    pub dll: String,
    pub name: String,
    pub func: Option<ApiFn>,
    /// Stdcall argument count when known (0 otherwise).
    pub nargs: u32,
    /// Bytes of arguments to pop instead of what the implementation reports (an alias
    /// with a longer argument list).
    pub pop: Option<u32>,
}

/// Writes a C string into a sized guest buffer (shared by every module).
pub fn kernel32_copy_str(c: &mut Ctx, buf: u32, cap: u32, s: &[u8]) -> u32 {
    kernel32::copy_str_out(c, buf, cap, s)
}

/// All APIs known by name plus every import that was requested (even if unimplemented).
#[derive(Default)]
pub struct Registry {
    pub entries: Vec<ApiEntry>,
    pub by_name: HashMap<(String, String), ApiId>,
    /// Math functions behind table-driven msvcrt entries, by API id.
    pub math_fns: HashMap<u32, fn(f64) -> f64>,
    /// APIs that do not end a clock spin-wait (clock reads, empty message polls).
    pub idle_ok: std::collections::HashSet<u32>,
    /// Exported variables (msvcrt's `_iob`...): their initial contents, built from
    /// the address they get (so they can point into themselves).
    pub data: HashMap<(String, String), (u32, fn(u32) -> Vec<u8>)>,
    /// Addresses handed out so far, and contents not yet written to guest memory.
    pub data_addr: HashMap<(String, String), u32>,
    pub data_next: u32,
    pub data_pending: Vec<(u32, Vec<u8>)>,
}

/// Exported variables live here, after the stubs.
pub const DATA_EXPORTS: u32 = crate::process::STUB_BASE + 0x0010_0000;
pub const DATA_EXPORTS_SIZE: u32 = 0x1_0000;

impl Registry {
    pub fn new() -> Self {
        let mut r = Registry::default();
        kernel32::register(&mut r);
        user32::register(&mut r);
        gdi32::register(&mut r);
        misc::register(&mut r);
        d3d8::register(&mut r);
        d3d9::register(&mut r);
        com::register(&mut r);
        dmusic::register(&mut r);
        msvcrt::register(&mut r);
        msvcrt::register_more(&mut r);
        dsound::register(&mut r);
        kernel32::register_more(&mut r);
        kernel32::register_paths(&mut r);
        kernel32::register_extra(&mut r);
        for (dll, name) in [
            ("kernel32.dll", "QueryPerformanceCounter"),
            ("kernel32.dll", "QueryPerformanceFrequency"),
            ("kernel32.dll", "GetTickCount"),
            ("kernel32.dll", "GetTickCount64"),
            ("kernel32.dll", "GetSystemTimeAsFileTime"),
            ("winmm.dll", "timeGetTime"),
            ("user32.dll", "PeekMessageA"),
            ("user32.dll", "PeekMessageW"),
            ("user32.dll", "GetQueueStatus"),
            ("user32.dll", "GetInputState"),
            ("kernel32.dll", "GetCurrentThreadId"),
        ] {
            if let Some(id) = r.lookup(dll, name) {
                r.idle_ok.insert(id.0);
            }
        }
        user32::register_more(&mut r);
        user32::register_extra(&mut r);
        r
    }

    pub fn add(&mut self, dll: &str, name: &str, f: ApiFn) -> ApiId {
        self.add_n(dll, name, 0, f)
    }

    pub fn add_n(&mut self, dll: &str, name: &str, nargs: u32, f: ApiFn) -> ApiId {
        let key = (dll.to_lowercase(), name.to_string());
        if let Some(&id) = self.by_name.get(&key) {
            self.entries[id.0 as usize].func = Some(f);
            self.entries[id.0 as usize].nargs = nargs;
            return id;
        }
        let id = ApiId(self.entries.len() as u32);
        self.entries.push(ApiEntry { dll: key.0.clone(), name: key.1.clone(), func: Some(f), nargs, pop: None });
        self.by_name.insert(key, id);
        id
    }

    /// Alias with its own stdcall argument count: the implementation reads the leading
    /// arguments it knows, and `nargs` are popped (e.g. CompareStringEx -> CompareStringW).
    pub fn alias_n(&mut self, dll: &str, name: &str, to_dll: &str, to_name: &str, nargs: u32) {
        if let Some(&id) = self.by_name.get(&(to_dll.to_lowercase(), to_name.to_string())) {
            if let Some(f) = self.entries[id.0 as usize].func {
                let new = self.add_n(dll, name, nargs, f);
                self.entries[new.0 as usize].pop = Some(nargs * 4);
            }
        }
    }

    /// Alias: another DLL/name mapped to an existing implementation.
    pub fn alias(&mut self, dll: &str, name: &str, to_dll: &str, to_name: &str) {
        if let Some(&id) = self.by_name.get(&(to_dll.to_lowercase(), to_name.to_string())) {
            if let Some(f) = self.entries[id.0 as usize].func {
                self.add(dll, name, f);
            }
        }
    }

    /// Looks up or creates an entry (possibly unimplemented) for an import.
    /// Registers an exported variable of `size` bytes.
    pub fn add_data(&mut self, dll: &str, name: &str, size: u32, init: fn(u32) -> Vec<u8>) {
        self.data.insert((dll.to_lowercase(), name.to_string()), (size, init));
    }

    /// The address of an exported variable, allocating it on first use.
    pub fn data_export(&mut self, dll: &str, name: &str) -> Option<u32> {
        let dll = dll.to_lowercase();
        let dll = if dll.ends_with(".dll") { dll } else { format!("{dll}.dll") };
        let key = (dll, name.to_string());
        if let Some(&a) = self.data_addr.get(&key) {
            return Some(a);
        }
        let &(size, init) = self.data.get(&key)?;
        let addr = DATA_EXPORTS + self.data_next;
        self.data_next = (self.data_next + size + 15) & !15;
        assert!(self.data_next <= DATA_EXPORTS_SIZE, "exported variables overflow their region");
        self.data_addr.insert(key, addr);
        self.data_pending.push((addr, init(addr)));
        Some(addr)
    }

    /// Writes the contents of variables bound since the last call.
    pub fn flush_data(&mut self, mem: &mut dyn Guest) {
        for (a, bytes) in self.data_pending.drain(..) {
            let _ = mem.ensure_mapped(DATA_EXPORTS, DATA_EXPORTS_SIZE);
            let _ = mem.write(a, &bytes);
        }
    }

    pub fn intern(&mut self, dll: &str, name: &str) -> ApiId {
        let dll = dll.to_lowercase();
        let dll = if dll.ends_with(".dll") || dll.ends_with(".drv") { dll } else { format!("{dll}.dll") };
        let key = (dll.clone(), name.to_string());
        if let Some(&id) = self.by_name.get(&key) {
            return id;
        }
        let id = ApiId(self.entries.len() as u32);
        self.entries.push(ApiEntry { dll, name: name.to_string(), func: None, nargs: 0, pop: None });
        self.by_name.insert(key, id);
        id
    }

    pub fn lookup(&self, dll: &str, name: &str) -> Option<ApiId> {
        let dll = dll.to_lowercase();
        let dll = if dll.ends_with(".dll") || dll.ends_with(".drv") { dll } else { format!("{dll}.dll") };
        self.by_name.get(&(dll, name.to_string())).copied()
    }

    pub fn name(&self, id: ApiId) -> String {
        let e = &self.entries[id.0 as usize];
        format!("{}!{}", e.dll, e.name)
    }

    pub fn func(&self, id: ApiId) -> Option<ApiFn> {
        self.entries.get(id.0 as usize).and_then(|e| e.func)
    }
}

/// Registers a batch of `(name, nargs, constant)` stubs that return a constant.
#[macro_export]
macro_rules! const_apis {
    ($r:expr, $dll:expr, { $($name:literal => ($n:expr, $v:expr)),* $(,)? }) => {
        $(
            $r.add($dll, $name, |_c| Ok($crate::hle::Ret::stdcall($n, $v)));
        )*
    };
}

/// ApiError text that ends a run normally once `Process::frame_limit` frames were shown.
pub const FRAME_LIMIT: &str = "frame limit reached";

pub const ERROR_SUCCESS: u32 = 0;
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_INVALID_HANDLE: u32 = 6;
pub const ERROR_NO_MORE_FILES: u32 = 18;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub const ERROR_MOD_NOT_FOUND: u32 = 126;
pub const ERROR_PROC_NOT_FOUND: u32 = 127;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_ENVVAR_NOT_FOUND: u32 = 203;
pub const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
pub const INVALID_HANDLE_VALUE: u32 = 0xFFFF_FFFF;
