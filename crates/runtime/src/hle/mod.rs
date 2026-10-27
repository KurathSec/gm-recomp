//! High-level emulation of Win32 APIs. Each API is a Rust function that reads its
//! stdcall arguments from the guest stack and returns the value for EAX plus how
//! many argument bytes the callee pops.

pub mod acm;
pub mod audio;
pub mod com;
pub mod d3d8;
pub mod d3d11;
pub mod d3d9;
pub mod d3d9_shader;
pub mod dxbc;
pub mod dinput;
pub mod dmusic;
pub mod dsound;
pub mod gpu;
pub mod gdi32;
pub mod gdiplus;
pub mod httpdll;
pub mod kernel32;
pub mod threads;
pub mod mci;
pub mod midi;
pub mod misc;
pub mod mmio;
pub mod shlwapi;
pub mod msvcrt;
pub mod nls;
pub mod pads;
pub mod profile;
pub mod seh64;
pub mod user32;
pub mod wasapi;
pub mod winsock;

use serde::{Deserialize, Serialize};
use crate::mem::Guest;
use crate::process::Process;
use crate::fasthash::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiId(pub u32);

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
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
    /// A 64-bit process's rax..r15 (encoding order). The fields above are then views of
    /// their low halves (`fs_base`: the GS base, the TEB), and writing one of them
    /// changes the register as a 32-bit write does: zero-extended (`engine::write_regs`).
    pub r64: Option<[u64; 16]>,
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
    /// High half of a 64-bit result: EDX, or in a 64-bit process the upper half of RAX
    /// (without it, RAX is EAX zero-extended, or sign-extended for -4096..-1: handles
    /// and LRESULTs such as INVALID_HANDLE_VALUE).
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
    /// A pointer-sized result: in a 64-bit process RAX gets the upper half too (an
    /// address in a 64-bit image, `Ctx::full`).
    pub fn ptr(nargs: u32, v: u64) -> Self {
        Ret { eax: v as u32, pop: nargs * 4, resume: false, edx: (v >> 32 != 0).then_some((v >> 32) as u32) }
    }
}

/// Error raised by an API: ends the run with a message.
#[derive(Debug)]
pub struct ApiError(pub String);

pub type ApiResult = Result<Ret, ApiError>;

/// What a dialog asks the player (`Host::prompt`).
#[derive(Clone, Copy, Debug)]
pub enum Ask<'a> {
    /// a message, with OK
    Message,
    /// a question answered yes or no
    Question,
    /// a line of text, with this default
    Text(&'a str),
    /// a runtime error of the game's code (GameMaker 8's error form): Some to ignore it and
    /// go on, None to abort the game
    Error,
}

/// Services the host provides to APIs beyond raw memory: calling back into guest code.
pub trait Host: Guest {
    /// Calls guest `addr` with stdcall `args`; returns EAX.
    fn call_guest(&mut self, p: &mut Process, regs: &mut Regs, addr: u32, args: &[u32]) -> Result<u32, String>;
    /// Calls guest `addr` by the Win64 convention (a 64-bit process); returns RAX.
    fn call_guest64(&mut self, p: &mut Process, regs: &mut Regs, addr: u32, args: &[u64]) -> Result<u64, String>;
    /// Ends execution of the guest (ExitProcess).
    fn request_exit(&mut self, code: u32);
    /// Pushes a value onto the x87 stack (FLD m64 semantics).
    fn fpu_push(&mut self, v: f64);
    /// Pops ST(0) (FSTP m64 semantics).
    fn fpu_pop(&mut self) -> f64;
    /// The calling thread's x87 state, and writing it back (control/status words).
    fn fpu_state(&mut self) -> fpu::Fpu;
    fn set_fpu_state(&mut self, f: &fpu::Fpu);
    /// XMM register `i` (0..16 in a 64-bit process), and writing it.
    fn xmm_reg(&mut self, i: u32) -> u128;
    fn set_xmm_reg(&mut self, i: u32, v: u128);
    /// SSE control/status register.
    fn mxcsr(&mut self) -> u32;
    fn set_mxcsr(&mut self, v: u32);
    /// The running secondary thread blocked in this API: return to the scheduler
    /// once it returns.
    fn yield_thread(&mut self);
    /// Lets the secondary threads that can run do so (the main thread is waiting).
    fn run_threads(&mut self, p: &mut Process);
    /// A General MIDI SoundFont, once available (the first call may start loading it).
    fn soundfont(&mut self) -> Option<Vec<u8>> {
        None
    }
    /// A system DLL the import did not supply (one a DLL the game writes at run time
    /// imports, such as an extension's ucrtbase.dll): the player's copy, if it has one.
    fn system_dll(&mut self, _name: &str) -> Option<Vec<u8>> {
        None
    }
    /// The main thread's clock moved on to `clock_us`: a host that plays in real time
    /// holds it there until real time catches up.
    fn pace(&mut self, _clock_us: u64) {}
    /// How long (ms) a host that plays in real time has before the guest's clock is
    /// due at `clock_us` (0: none, or not playing in real time).
    fn slack_ms(&mut self, _clock_us: u64) -> f64 {
        0.0
    }
    /// Whether the host can make network connections (the page: a relay is set); without,
    /// WSAStartup fails as it always did.
    fn net_enabled(&mut self) -> bool {
        false
    }
    /// A network connection for a guest socket (`winsock`): connection `id`, a TCP stream
    /// or UDP datagrams to `host`:`port`. False when the host has no network (the page:
    /// no relay set).
    fn net_open(&mut self, _id: u32, _udp: bool, _host: &str, _port: u16) -> bool {
        false
    }
    /// Sends (queued until the connection is open); false once it has ended.
    fn net_send(&mut self, _id: u32, _data: &[u8]) -> bool {
        false
    }
    /// Reads what arrived: TCP up to `buf.len()` bytes, UDP one datagram (cut to fit).
    /// Some(0) at the end of a TCP stream; None when nothing is there yet.
    fn net_recv(&mut self, _id: u32, _buf: &mut [u8]) -> Option<usize> {
        Some(0)
    }
    /// Bytes waiting (UDP: the next datagram's size).
    fn net_available(&mut self, _id: u32) -> u32 {
        0
    }
    fn net_state(&mut self, _id: u32) -> winsock::NetState {
        winsock::NetState::Failed
    }
    fn net_close(&mut self, _id: u32) {}
    /// Asks the player (`Ask`): the text typed, Some("") for OK or Yes, None when cancelled
    /// (or No). The game waits meanwhile, as it does on a Windows dialog.
    fn prompt(&mut self, _ask: Ask, _title: &str, _text: &str) -> Option<String> {
        None
    }
    /// Input that arrived since the last call (keyboard, mouse); `frame` is the number
    /// of frames presented so far, `clock_us` the guest's clock (a replay delivers each
    /// event at the poll it was taken at).
    fn poll_input(&mut self, _frame: u64, _clock_us: u64) -> Vec<InputEvent> {
        Vec::new()
    }
    /// Files another copy of this game wrote since the last call ((path as stored,
    /// contents or None if removed)): a standby copy of the player's (web/src/player.mjs)
    /// keeps the files the game has, as it takes over at a load.
    fn take_files(&mut self) -> Vec<(String, Option<Vec<u8>>)> {
        Vec::new()
    }
    /// Whether `monitor` lines are wanted (a session that can be replayed).
    fn monitoring(&self) -> bool {
        false
    }
    /// A line for whoever watches the session live (the input as the guest took it).
    fn monitor(&mut self, _line: &str) {}
}

#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    /// Virtual-key code, scan code (with 0x100 for extended keys; 0x200 when the
    /// host's NumLock is on, which decides what the numeric keypad sends), pressed.
    Key { vk: u8, scan: u16, down: bool },
    /// Client coordinates of the game window and the button state (bit 0 left,
    /// bit 1 right, bit 2 middle).
    Mouse { x: i32, y: i32, buttons: u32 },
    /// Game controller `index` (0..4) in XInput terms: XINPUT_GAMEPAD button bits,
    /// triggers 0..255, thumb sticks (LX, LY, RX, RY; up and right positive).
    Pad { index: u8, connected: bool, buttons: u16, triggers: [u8; 2], thumbs: [i16; 4] },
}

/// A game controller as XInput and the joystick API report it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PadState {
    pub connected: bool,
    pub buttons: u16,
    pub triggers: [u8; 2],
    pub thumbs: [i16; 4],
    /// XINPUT_STATE.dwPacketNumber: changes with the state.
    pub packet: u32,
}

pub struct Ctx<'a> {
    pub host: &'a mut dyn Host,
    pub regs: &'a mut Regs,
    pub p: &'a mut Process,
    pub id: ApiId,
}

impl Ctx<'_> {
    /// Argument `i` (0-based): stdcall from the guest stack, or in a 64-bit process the
    /// low half of the Win64 argument (an address's low half is where it is in guest
    /// memory).
    pub fn arg(&self, i: u32) -> u32 {
        if self.regs.r64.is_some() {
            return self.arg64(i) as u32;
        }
        self.host.read_u32(self.regs.esp + 4 + 4 * i).unwrap_or(0)
    }
    /// Win64 argument `i`: rcx, rdx, r8, r9, then the stack above the return address and
    /// the 32 bytes of shadow space. In a 32-bit process, the stdcall argument.
    pub fn arg64(&self, i: u32) -> u64 {
        match &self.regs.r64 {
            Some(r) if i < 4 => r[[1, 2, 8, 9][i as usize]],
            Some(r) => self.host.read_u64(r[4] as u32 + 0x28 + 8 * (i - 4)).unwrap_or(0),
            None => self.arg(i) as u64,
        }
    }
    /// Where the variadic arguments from argument `first` on are in guest memory: on the
    /// stack (4-byte slots), or in a 64-bit process the home area, where the register
    /// arguments are stored first as a variadic callee does, so all are 8-byte slots in
    /// a row.
    pub fn varargs(&mut self, first: u32) -> u32 {
        let Some(r) = self.regs.r64 else { return self.regs.esp + 4 + 4 * first };
        let rsp = r[4] as u32;
        for (k, reg) in [1, 2, 8, 9].into_iter().enumerate() {
            let _ = self.host.write_u64(rsp + 8 + 8 * k as u32, r[reg]);
        }
        rsp + 8 + 8 * first
    }
    /// A `float` argument `i`: in a 64-bit process the first four go in xmm0..3 by
    /// position, not in the integer registers.
    pub fn arg_f32(&mut self, i: u32) -> f32 {
        if self.x64() && i < 4 {
            return f32::from_bits(self.host.xmm_reg(i) as u32);
        }
        f32::from_bits(self.arg(i))
    }
    /// A 64-bit process: pointers, handles and `SIZE_T` are 8 bytes in structures.
    pub fn x64(&self) -> bool {
        self.regs.r64.is_some()
    }
    /// Size of a pointer in guest structures.
    pub fn ptr_size(&self) -> u32 {
        if self.x64() { 8 } else { 4 }
    }
    /// A pointer-sized field (its low half in a 64-bit process: where it points in guest
    /// memory).
    pub fn read_ptr(&self, addr: u32) -> u32 {
        self.host.read_u32(addr).unwrap_or(0)
    }
    /// Writes a pointer-sized field (zero-extended in a 64-bit process).
    pub fn write_ptr(&mut self, addr: u32, v: u32) {
        if self.x64() {
            let _ = self.host.write_u64(addr, v as u64);
        } else {
            let _ = self.host.write_u32(addr, v);
        }
    }
    /// Writes a pointer-sized signed field (LONG_PTR, LRESULT: sign-extended).
    pub fn write_iptr(&mut self, addr: u32, v: i32) {
        if self.x64() {
            let _ = self.host.write_u64(addr, v as i64 as u64);
        } else {
            let _ = self.host.write_u32(addr, v as u32);
        }
    }
    /// An address as the guest sees it: in a 64-bit process, one inside a 64-bit image
    /// gets its upper half back (APIs keep the low halves, where things are in guest
    /// memory; nothing else lives at those low addresses).
    pub fn full(&self, a: u32) -> u64 {
        if self.x64() { self.p.full_addr(a) } else { a as u64 }
    }
    /// TEB field offsets that differ between the 32- and 64-bit layouts.
    pub fn teb_off(&self, f: TebField) -> u32 {
        f.offset(self.x64())
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
        let _ = self.host.write_u32(teb + self.teb_off(TebField::LastError), e);
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
    /// `call_guest` with pointer-sized arguments (full 64-bit ones in a 64-bit process,
    /// where RAX comes back whole; truncated to 32 bits otherwise).
    pub fn call_guest64(&mut self, addr: u32, args: &[u64]) -> Result<u64, ApiError> {
        if !self.x64() {
            let args: Vec<u32> = args.iter().map(|&a| a as u32).collect();
            return self.call_guest(addr, &args).map(|v| v as u64);
        }
        self.host.call_guest64(self.p, self.regs, addr, args).map_err(ApiError)
    }
    /// Advances the virtual clock (sleeps, idle waits) and everything driven by it.
    pub fn advance_clock(&mut self, us: u64) {
        self.p.clock_us += us;
        // Real time is kept where the main thread's clock moves on (at most once a
        // millisecond), so threads waking meanwhile run in that idle time, not within
        // the next frame.
        if self.p.current == 0 && self.p.clock_us / 1000 != self.p.paced_ms {
            self.p.paced_ms = self.p.clock_us / 1000;
            // The sound mixed so far goes out now, not only at the next Present: the
            // host's buffer can stay small and a sound starts sooner after its frame.
            dsound::flush_audio(self);
            // A texture the guest finished writing goes to the GPU here if this wait
            // leaves time for it, instead of in the frame that first draws it (GMS
            // decodes every texture page at the start; a room's first draw from a
            // 2048x2048 page used to upload it then).
            if !self.p.pending_uploads.is_empty() && self.host.slack_ms(self.p.clock_us) > UPLOAD_SLACK_MS {
                let (tex, version) = self.p.pending_uploads.remove(0);
                if version == 8 {
                    d3d8::upload_pending(self, tex);
                } else {
                    d3d9::upload_pending(self, tex);
                }
            }
            self.host.pace(self.p.clock_us);
        }
        dsound::tick(self);
        // Time passed for the main thread: threads whose wait ended run now.
        if self.p.current == 0 && self.p.threads.len() > 1 {
            self.host.run_threads(self.p);
        }
    }
    /// A clock read. Back-to-back reads with no other work in between (a spin-wait on
    /// the time) advance the clock in doubling steps of 1..4 µs, so polling loops end
    /// deterministically, close to their deadline: GMS 1.4 spins the last ~11 ms of
    /// every frame and measures the next frame from where the spin ended, so coarser
    /// steps slow it down (128 µs steps ran it at 49 fps).
    pub fn poll_clock(&mut self) {
        // A secondary thread that keeps waiting on the clock without ever blocking (a loop
        // of sleeps that spin, which on Windows runs beside the main thread on another
        // core) no longer moves it, and every few reads sleeps a little while the others
        // run: it is preempted only where it returns to the dispatcher, which such a loop
        // never does. Time passes with the main thread. (Threads that read the clock and
        // block now and then keep moving it as the main thread does.)
        let spinning = self.p.current != 0 && Some(self.p.nest) == self.p.slice_nest && {
            let t = self.p.current_thread_mut();
            t.spin_polls = t.spin_polls.saturating_add(1);
            t.spin_polls > SPINNER_POLLS
        };
        if spinning {
            self.p.slice_polls += 1;
            if self.p.slice_polls >= SPIN_POLLS {
                self.p.slice_polls = 0;
                let until = self.p.clock_us + SPIN_SLEEP_US;
                self.p.current_thread_mut().state = crate::process::ThreadState::Sleeping(until);
                self.host.yield_thread();
            }
            return;
        }
        let step = 1u64 << self.p.idle_streak.min(2);
        self.p.idle_streak += 1;
        self.advance_clock(step);
    }
    /// Declared argument count of the current API (for tables of uniform methods).
    pub fn nargs(&self) -> u32 {
        self.p.registry.entries.get(self.id.0 as usize).map(|e| e.nargs).unwrap_or(0)
    }
}

/// TEB fields whose place differs between the 32- and 64-bit layouts.
#[derive(Clone, Copy)]
pub enum TebField {
    ExceptionList,
    StackBase,
    StackLimit,
    Self_,
    ProcessId,
    ThreadId,
    TlsPointer,
    Peb,
    LastError,
    TlsSlots,
    TlsExpansion,
}

impl TebField {
    pub fn offset(self, x64: bool) -> u32 {
        use TebField::*;
        match (self, x64) {
            (ExceptionList, _) => 0,
            (StackBase, false) => 0x04,
            (StackBase, true) => 0x08,
            (StackLimit, false) => 0x08,
            (StackLimit, true) => 0x10,
            (Self_, false) => 0x18,
            (Self_, true) => 0x30,
            (ProcessId, false) => 0x20,
            (ProcessId, true) => 0x40,
            (ThreadId, false) => 0x24,
            (ThreadId, true) => 0x48,
            (TlsPointer, false) => 0x2C,
            (TlsPointer, true) => 0x58,
            (Peb, false) => 0x30,
            (Peb, true) => 0x60,
            (LastError, false) => 0x34,
            (LastError, true) => 0x68,
            (TlsSlots, false) => 0xE10,
            (TlsSlots, true) => 0x1480,
            (TlsExpansion, false) => 0xF94,
            (TlsExpansion, true) => 0x1780,
        }
    }
}

/// Clock reads a secondary thread makes before it sleeps, and for how long (µs)
/// (`Ctx::poll_clock`).
const SPIN_POLLS: u32 = 8;
const SPIN_SLEEP_US: u64 = 500;
/// Clock reads without blocking that make a secondary thread such a spinner.
const SPINNER_POLLS: u32 = 1000;

/// The time a wait must leave (ms) for a texture upload in it (a 2048x2048 page takes
/// several ms in a browser).
const UPLOAD_SLACK_MS: f64 = 12.0;

pub type ApiFn = fn(&mut Ctx) -> ApiResult;

impl ApiEntry {
    pub fn clone_name(&self) -> String {
        format!("{}!{}", self.dll, self.name)
    }
}

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
    pub idle_ok: std::collections::HashSet<u32, crate::fasthash::AddrHash>,
    /// Exported variables (msvcrt's `_iob`...): their initial contents, built from
    /// the address they get (so they can point into themselves).
    pub data: HashMap<(String, String), (u32, fn(u32) -> Vec<u8>)>,
    /// Addresses handed out so far, and contents not yet written to guest memory.
    pub data_addr: HashMap<(String, String), u32>,
    pub data_next: u32,
    pub data_pending: Vec<(u32, Vec<u8>)>,
    /// Implementations an API gets only when a module imports it (`add_on_demand`): APIs
    /// added after the rest, so the ids (stub addresses, and with them the state games
    /// see) of everything that games do import stay where they were.
    pub on_demand: HashMap<(String, String), ApiFn>,
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
        mmio::register(&mut r);
        shlwapi::register(&mut r);
        winsock::register(&mut r);
        httpdll::register(&mut r);
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
        acm::register(&mut r);
        d3d11::register(&mut r);
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
        threads::register(&mut r);
        pads::register(&mut r);
        dinput::register(&mut r);
        profile::register(&mut r);
        // APIs added since, made when imported (`on_demand`)
        msvcrt::register_late(&mut r);
        gdiplus::register(&mut r);
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
        let func = self.on_demand.get(&key).copied();
        self.entries.push(ApiEntry { dll, name: name.to_string(), func, nargs: 0, pop: None });
        self.by_name.insert(key, id);
        id
    }

    /// `intern` for an import or GetProcAddress: kernelbase.dll holds what kernel32.dll
    /// and advapi32.dll forward to, so it is theirs when they have it; crtdll.dll, the
    /// C runtime before msvcrt.dll, shares its functions.
    pub fn intern_import(&mut self, dll: &str, name: &str) -> ApiId {
        let stem = dll.to_lowercase();
        let stem = stem.trim_end_matches(".dll");
        let to: &[&str] = match stem {
            "kernelbase" => &["kernel32.dll", "advapi32.dll", "user32.dll"],
            "crtdll" => &["msvcrt.dll"],
            _ => &[],
        };
        if let Some(d) = to.iter().find(|d| self.knows(d, name)) {
            return self.intern(d, name);
        }
        self.intern(dll, name)
    }

    /// `dll!name` is implemented, or will be when imported (`on_demand`).
    pub fn knows(&self, dll: &str, name: &str) -> bool {
        let key = (dll.to_lowercase(), name.to_string());
        self.by_name.get(&key).is_some_and(|&id| self.entries[id.0 as usize].func.is_some()) || self.on_demand.contains_key(&key)
    }

    /// `f` implements `dll!name` once a module imports it (see `on_demand`).
    pub fn add_on_demand(&mut self, dll: &str, name: &str, f: ApiFn) {
        self.on_demand.insert((dll.to_lowercase(), name.to_string()), f);
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

    /// What a save state keeps of the registry (`RegistryState`).
    pub fn state(&self) -> RegistryState {
        let mut data_addr: Vec<((String, String), u32)> = self.data_addr.iter().map(|(k, &a)| (k.clone(), a)).collect();
        data_addr.sort();
        RegistryState {
            names: self.entries.iter().map(|e| (e.dll.clone(), e.name.clone())).collect(),
            data_addr,
            data_next: self.data_next,
            data_pending: self.data_pending.clone(),
        }
    }

    /// The APIs both have agree (an id is a stub address the guest holds).
    pub fn check(&self, s: &RegistryState) -> Result<(), String> {
        for (i, (e, (dll, name))) in self.entries.iter().zip(&s.names).enumerate() {
            if e.dll != *dll || e.name != *name {
                return Err(format!("API {i} is {}!{} here but {dll}!{name} in the state (another version of the player)", e.dll, e.name));
            }
        }
        Ok(())
    }

    /// Makes the ids what they were in the state: APIs it named since start are named
    /// again (`intern`), those named here since are dropped.
    pub fn restore(&mut self, s: &RegistryState) -> Result<(), String> {
        self.check(s)?;
        while self.entries.len() > s.names.len() {
            let id = self.entries.len() as u32 - 1;
            let e = self.entries.pop().unwrap();
            let key = (e.dll, e.name);
            if self.by_name.get(&key) == Some(&ApiId(id)) {
                self.by_name.remove(&key);
            }
            self.idle_ok.remove(&id);
            self.math_fns.remove(&id);
        }
        for (i, (dll, name)) in s.names.iter().enumerate().skip(self.entries.len()) {
            let id = self.intern(dll, name);
            if id.0 as usize != i {
                return Err(format!("{dll}!{name} cannot be API {i} again"));
            }
        }
        self.data_addr = s.data_addr.iter().cloned().collect();
        self.data_next = s.data_next;
        self.data_pending = s.data_pending.clone();
        Ok(())
    }
}

/// The registry in a save state: each id's API (`dll`, `name`) and the exported
/// variables handed out.
#[derive(Serialize, Deserialize)]
pub struct RegistryState {
    pub names: Vec<(String, String)>,
    pub data_addr: Vec<((String, String), u32)>,
    pub data_next: u32,
    pub data_pending: Vec<(u32, Vec<u8>)>,
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

/// Every COM class (its interfaces, primary first), for a save state to find its
/// objects' classes again by their interfaces' names.
pub(crate) fn com_classes() -> Vec<&'static [&'static com::Interface]> {
    vec![
        &com::WEBBROWSER,
        &dinput::DI8A_CLASS,
        &dinput::DI8W_CLASS,
        &dinput::DEVICE_A_CLASS,
        &dinput::DEVICE_W_CLASS,
        &dmusic::SEGMENT_CLASS,
        &dmusic::SEGMENT_STATE_CLASS,
        &dmusic::AUDIOPATH_CLASS,
        &dmusic::PATH_BUFFER_CLASS,
        &dmusic::PERFORMANCE_CLASS,
        &dmusic::LOADER_CLASS,
        &dsound::DEVICE_CLASS,
        &dsound::BUFFER_CLASS,
        &wasapi::ENUMERATOR_CLASS,
        &wasapi::COLLECTION_CLASS,
        &wasapi::DEVICE_CLASS,
        &wasapi::PROPERTIES_CLASS,
        &wasapi::CLIENT_CLASS,
        &wasapi::RENDER_CLASS,
        &wasapi::CLOCK_CLASS,
        &wasapi::VOLUME_CLASS,
    ]
}
