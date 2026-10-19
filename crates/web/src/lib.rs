//! The browser host: the engine compiled to wasm32, driven by a JS worker.
//!
//! Guest memory and the CPU-state globals live in a small "cpu" module generated here
//! (`glue_module`): it defines them, imports this module's memory, and exports
//! accessors (cross-memory `memory.copy`, typed global get/set). JS puts those
//! accessors and every translated function into this module's function table, so all
//! calls between the engine, the accessors and translated code are direct wasm calls.

#![cfg(target_arch = "wasm32")]

use recomp::abi::{self, GLOBALS, Global};
use runtime::engine::{self, Engine, Machine};
use runtime::mem::Fault;
use runtime::vfs::MemFs;
use runtime::{Guest, Process};

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    /// Compiles and instantiates a translated module; writes the table index of each
    /// export named in `names` (newline-separated) to `out`. Returns 1 on success.
    fn instantiate(wasm: *const u8, len: usize, names: *const u8, names_len: usize, out: *mut u32) -> i32;
    fn print(ptr: *const u8, len: usize);
    fn now_us() -> f64;
}

#[link(wasm_import_module = "gpu")]
unsafe extern "C" {
    fn gpu_resize(w: u32, h: u32);
    fn gpu_texture_create(id: u32, w: u32, h: u32, levels: u32, fmt: u32, rt: u32);
    fn gpu_texture_upload(id: u32, level: u32, w: u32, h: u32, fmt: u32, pitch: u32, bits: u32);
    fn gpu_texture_delete(id: u32);
    fn gpu_set_target(id: u32, w: u32, h: u32);
    fn gpu_clear(flags: u32, color: u32, z: f32, stencil: u32, x: i32, y: i32, w: i32, h: i32);
    fn gpu_draw(call: *const runtime::hle::gpu::DrawCall);
    fn gpu_draw9(call: *const runtime::hle::gpu::DrawCall9);
    fn gpu_shader_create(id: u32, vertex: u32, glsl: *const u8, len: usize);
    fn gpu_shader_delete(id: u32);
    fn gpu_blit(src: u32, sx: i32, sy: i32, sw: i32, sh: i32, dst: u32, dx: i32, dy: i32, dw: i32, dh: i32, linear: u32);
    fn gpu_read_target(id: u32, w: u32, h: u32, dst: u32, pitch: u32);
    /// Shows the frame; may suspend the guest until it is time for the next one.
    fn gpu_present(clock_ms: f64, x: i32, y: i32, w: i32, h: i32, dst_w: u32, dst_h: u32);
    /// Holds the guest until real time reaches its clock (may suspend it).
    fn gpu_pace(clock_ms: f64);
    /// How long (ms) until real time reaches the clock (0 when not pacing).
    fn gpu_slack(clock_ms: f64) -> f64;
}

/// Guest sockets' connections (hle/winsock.rs), relayed by the page over WebSockets.
#[link(wasm_import_module = "net")]
unsafe extern "C" {
    /// 1 when the page has a relay for connections.
    fn net_enabled() -> u32;
    /// 1 when the page can make the connection (a relay is set).
    fn net_open(id: u32, udp: u32, host: *const u8, host_len: usize, port: u32) -> u32;
    /// 0 once the connection has ended.
    fn net_send(id: u32, data: *const u8, len: usize) -> u32;
    /// Bytes read (0: the end of a TCP stream), or -1 when nothing has arrived.
    fn net_recv(id: u32, buf: *mut u8, len: usize) -> i32;
    fn net_available(id: u32) -> u32;
    /// 0 connecting, 1 open, 2 closed, 3 failed
    fn net_state(id: u32) -> u32;
    fn net_close(id: u32);
}

#[link(wasm_import_module = "audio")]
unsafe extern "C" {
    /// Interleaved stereo f32 frames at `rate`.
    fn audio_push(samples: *const f32, len: usize, rate: u32);
}

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    /// Changed files, serialized as by `WebSaves`.
    fn fs_persist(data: *const u8, len: usize);
    /// Starts fetching the MIDI SoundFont; it arrives through `soundfont_add`.
    fn soundfont_request();
    /// Hands over the key events the page wrote to the shared input ring since the
    /// last call (through `input_key`).
    fn input_drain();
}

/// Hands the game's file changes to the page (web/src/host.mjs `onSave`): per entry
/// u32 path length, path (UTF-8), i32 data length (-1: removed), data, f64 last-write
/// time (ms since 1970).
struct WebSaves;

impl runtime::vfs::SaveSink for WebSaves {
    fn save(&mut self, changes: &[(String, Option<(Vec<u8>, u64)>)]) {
        let mut out = Vec::new();
        for (path, data) in changes {
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            match data {
                Some((d, filetime)) => {
                    out.extend_from_slice(&(d.len() as i32).to_le_bytes());
                    out.extend_from_slice(d);
                    let ms = *filetime as f64 / 10_000.0 - 11_644_473_600_000.0;
                    out.extend_from_slice(&ms.to_le_bytes());
                }
                None => {
                    out.extend_from_slice(&(-1i32).to_le_bytes());
                    out.extend_from_slice(&0f64.to_le_bytes());
                }
            }
        }
        unsafe { fs_persist(out.as_ptr(), out.len()) }
    }
}

/// The audio ring in web/src/host.mjs.
struct WebAudio;

impl runtime::hle::audio::AudioSink for WebAudio {
    fn push(&mut self, samples: &[f32], rate: u32) {
        unsafe { audio_push(samples.as_ptr(), samples.len(), rate) }
    }
}

/// The WebGL2 renderer in web/src/gpu.mjs.
struct WebGpu;

impl runtime::hle::gpu::Gpu for WebGpu {
    fn resize(&mut self, w: u32, h: u32) {
        unsafe { gpu_resize(w, h) }
    }
    fn texture_create(&mut self, id: u32, w: u32, h: u32, levels: u32, fmt: u32, rt: bool) {
        unsafe { gpu_texture_create(id, w, h, levels, fmt, rt as u32) }
    }
    fn texture_upload(&mut self, id: u32, level: u32, w: u32, h: u32, fmt: u32, pitch: u32, bits: u32) {
        unsafe { gpu_texture_upload(id, level, w, h, fmt, pitch, bits) }
    }
    fn draw9(&mut self, d: &runtime::hle::gpu::DrawCall9) {
        unsafe { gpu_draw9(d) }
    }
    fn shader_create(&mut self, id: u32, vertex: bool, glsl: &str) {
        unsafe { gpu_shader_create(id, vertex as u32, glsl.as_ptr(), glsl.len()) }
    }
    fn shader_delete(&mut self, id: u32) {
        unsafe { gpu_shader_delete(id) }
    }
    fn blit(&mut self, src: u32, s: [i32; 4], dst: u32, d: [i32; 4], linear: bool) {
        unsafe { gpu_blit(src, s[0], s[1], s[2], s[3], dst, d[0], d[1], d[2], d[3], linear as u32) }
    }
    fn read_target(&mut self, id: u32, w: u32, h: u32, dst: u32, pitch: u32) {
        unsafe { gpu_read_target(id, w, h, dst, pitch) }
    }
    fn texture_delete(&mut self, id: u32) {
        unsafe { gpu_texture_delete(id) }
    }
    fn set_target(&mut self, id: u32, w: u32, h: u32) {
        unsafe { gpu_set_target(id, w, h) }
    }
    fn clear(&mut self, flags: u32, color: u32, z: f32, stencil: u32, r: [i32; 4]) {
        unsafe { gpu_clear(flags, color, z, stencil, r[0], r[1], r[2], r[3]) }
    }
    fn draw(&mut self, d: &runtime::hle::gpu::DrawCall) {
        unsafe { gpu_draw(d) }
    }
    fn present(&mut self, clock_us: u64, src: [i32; 4], dst_w: u32, dst_h: u32) {
        unsafe { gpu_present(clock_us as f64 / 1000.0, src[0], src[1], src[2], src[3], dst_w, dst_h) }
    }
}

/// Without rendering (Node), the host still hears of each Present, for scripted input.
struct FrameGpu;

impl runtime::hle::gpu::Gpu for FrameGpu {
    fn present(&mut self, clock_us: u64, src: [i32; 4], dst_w: u32, dst_h: u32) {
        unsafe { gpu_present(clock_us as f64 / 1000.0, src[0], src[1], src[2], src[3], dst_w, dst_h) }
    }
}

/// Guest address space limit (the stub and marker pages sit just below 2 GiB).
const GUEST_MAX_PAGES: u64 = 32768;

#[derive(Clone, Copy)]
struct Glue {
    g2r: extern "C" fn(u32, *mut u8, u32),
    r2g: extern "C" fn(*const u8, u32, u32),
    fill: extern "C" fn(u32, u32, u32),
    get: extern "C" fn(u32) -> u32,
    set: extern "C" fn(u32, u32),
    size: extern "C" fn() -> u32,
    grow: extern "C" fn(u32) -> i32,
    /// XMM register i <-> 16 bytes of runtime memory.
    vget: extern "C" fn(u32, *mut u8),
    vset: extern "C" fn(u32, *const u8),
    /// Calls the translated function at a table index with the registers in the
    /// globals, and puts the ones it returns there.
    call: extern "C" fn(u32),
    /// gcopy(dst, src, len) within guest memory.
    gcopy: extern "C" fn(u32, u32, u32),
    /// The registers of `Regs` (in its order) to / from runtime memory.
    rget: extern "C" fn(*mut u32),
    rset: extern "C" fn(*const u32),
}

/// The CPU globals of `Regs`, in its field order.
const REG_GLOBALS: [Global; 10] =
    [Global::Eax, Global::Ecx, Global::Edx, Global::Ebx, Global::Esp, Global::Ebp, Global::Esi, Global::Edi, Global::Eip, Global::FsBase];

/// Export order of the glue accessors; JS places them in the table in this order.
const GLUE_FUNCS: [&str; 13] = ["g2r", "r2g", "fill", "get", "set", "size", "grow", "vget", "vset", "call", "gcopy", "rget", "rset"];

struct State {
    engine: Option<Engine>,
    /// Input from the page, applied by the HLE layer at its next message poll.
    input: Vec<runtime::InputEvent>,
    glue: Option<Glue>,
    fs: Option<MemFs>,
    started: Option<engine::Started>,
    guest_bytes: u64,
    /// The MIDI SoundFont: asked for, and the bytes until the runtime takes them.
    soundfont_requested: bool,
    soundfont: Option<Vec<u8>>,
    /// A recorded session's input: (frame, guest clock, event).
    replay: std::collections::VecDeque<(u64, u64, runtime::InputEvent)>,
    /// When the key ring was last read (frame, guest clock), and whether the host
    /// waited for real time since.
    drained: (u64, u64),
    waited: bool,
    polls: u32,
}

static mut STATE: State = State { input: Vec::new(), engine: None, glue: None, fs: None, started: None, guest_bytes: 0, soundfont_requested: false, soundfont: None, replay: std::collections::VecDeque::new(), drained: (u64::MAX, 0), waited: false, polls: 0 };

#[allow(static_mut_refs)]
fn state() -> &'static mut State {
    // Single-threaded: one worker owns this instance.
    unsafe { &mut STATE }
}

fn glue() -> Glue {
    state().glue.expect("glue not installed")
}

fn log(s: &str) {
    unsafe { print(s.as_ptr(), s.len()) }
}

struct Web;

impl Web {
    fn in_bounds(&self, addr: u32, len: usize) -> bool {
        let end = addr as u64 + len as u64;
        if end <= state().guest_bytes {
            return true;
        }
        state().guest_bytes = (glue().size)() as u64 * 65536;
        end <= state().guest_bytes
    }
}

impl Guest for Web {
    fn read(&self, addr: u32, buf: &mut [u8]) -> Result<(), Fault> {
        if !self.in_bounds(addr, buf.len()) {
            return Err(Fault { addr, write: false });
        }
        (glue().g2r)(addr, buf.as_mut_ptr(), buf.len() as u32);
        Ok(())
    }
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), Fault> {
        if !self.in_bounds(addr, data.len()) {
            return Err(Fault { addr, write: true });
        }
        (glue().r2g)(data.as_ptr(), addr, data.len() as u32);
        Ok(())
    }
    fn ensure_mapped(&mut self, addr: u32, size: u32) -> Result<(), Fault> {
        let end = addr as u64 + size as u64;
        let have = (glue().size)() as u64 * 65536;
        if end > have {
            let pages = (end - have).div_ceil(65536);
            if have / 65536 + pages > GUEST_MAX_PAGES || (glue().grow)(pages as u32) < 0 {
                return Err(Fault { addr, write: true });
            }
            state().guest_bytes = (glue().size)() as u64 * 65536;
        }
        Ok(())
    }
    fn fill(&mut self, addr: u32, size: u32, byte: u8) -> Result<(), Fault> {
        if !self.in_bounds(addr, size as usize) {
            return Err(Fault { addr, write: true });
        }
        (glue().fill)(addr, byte as u32, size);
        Ok(())
    }
    fn copy_within(&mut self, dst: u32, src: u32, size: u32) -> Result<(), Fault> {
        if !self.in_bounds(src, size as usize) {
            return Err(Fault { addr: src, write: false });
        }
        if !self.in_bounds(dst, size as usize) {
            return Err(Fault { addr: dst, write: true });
        }
        (glue().gcopy)(dst, src, size);
        Ok(())
    }
}

impl Machine for Web {
    fn engine(&mut self) -> &mut Engine {
        state().engine.as_mut().expect("engine not initialised")
    }
    fn engine_ref(&self) -> &Engine {
        state().engine.as_ref().expect("engine not initialised")
    }
    fn global(&mut self, g: Global) -> u32 {
        (glue().get)(g as u32)
    }
    fn set_global(&mut self, g: Global, v: u32) {
        (glue().set)(g as u32, v)
    }
    fn regs(&mut self) -> runtime::hle::Regs {
        let mut r = [0u32; 10];
        (glue().rget)(r.as_mut_ptr());
        runtime::hle::Regs { eax: r[0], ecx: r[1], edx: r[2], ebx: r[3], esp: r[4], ebp: r[5], esi: r[6], edi: r[7], eip: r[8], fs_base: r[9] }
    }
    fn set_regs(&mut self, x: &runtime::hle::Regs) {
        let r = [x.eax, x.ecx, x.edx, x.ebx, x.esp, x.ebp, x.esi, x.edi, x.eip, x.fs_base];
        (glue().rset)(r.as_ptr());
    }
    fn xmm(&mut self, i: u32) -> u128 {
        let mut b = [0u8; 16];
        (glue().vget)(i, b.as_mut_ptr());
        u128::from_le_bytes(b)
    }
    fn set_xmm(&mut self, i: u32, v: u128) {
        let b = v.to_le_bytes();
        (glue().vset)(i, b.as_ptr());
    }
    fn call_index(&mut self, idx: u32) -> Result<(), String> {
        // Translated functions take and return the registers: the glue's `call` passes
        // them from and to the globals.
        (glue().call)(idx);
        Ok(())
    }
    fn instantiate(&mut self, wasm: &[u8], exports: &[String]) -> Result<Vec<u32>, String> {
        let names = exports.join("\n");
        let mut out = vec![0u32; exports.len()];
        let ok = unsafe { instantiate(wasm.as_ptr(), wasm.len(), names.as_ptr(), names.len(), out.as_mut_ptr()) };
        if ok == 1 { Ok(out) } else { Err("module rejected by the host (see console)".into()) }
    }
    fn now_us(&self) -> u64 {
        unsafe { now_us() as u64 }
    }
    fn print(&mut self, s: &str) {
        log(s);
    }
    fn take_input(&mut self, frame: u64, clock_us: u64) -> Vec<runtime::InputEvent> {
        // The key ring is read through a call out to JS; games poll their messages
        // hundreds of times a frame while they spin on the clock, between real-time
        // waits. It is read after every wait (when keys come in), at a new frame, and
        // every 100 µs of guest time or 64 polls otherwise (a loop of bare message
        // polls moves no clock).
        let st = state();
        st.polls += 1;
        if st.waited || frame != st.drained.0 || clock_us >= st.drained.1 + 100 || st.polls >= 64 {
            st.drained = (frame, clock_us);
            st.waited = false;
            st.polls = 0;
            unsafe { input_drain() };
        }
        let mut out = std::mem::take(&mut state().input);
        // a replayed session: each event at the poll it was taken at
        let r = &mut state().replay;
        while let Some(&(f, t, _)) = r.front() {
            if frame < f || frame == f && clock_us < t {
                break;
            }
            out.push(r.pop_front().unwrap().2);
        }
        out
    }
    fn slack_ms(&mut self, clock_us: u64) -> f64 {
        unsafe { gpu_slack(clock_us as f64 / 1000.0) }
    }
    fn pace(&mut self, clock_us: u64) {
        let ms = clock_us as f64 / 1000.0;
        // Time to wait anyway (often a millisecond at a time: game threads sleep in
        // short steps): translate code the game has not reached yet.
        let slack = unsafe { gpu_slack(ms) };
        if slack > 0.8 {
            // most of it, at most 6 ms: a compile can take longer than estimated
            let deadline = self.now_us() + ((slack - 0.3) * 600.0).min(6000.0) as u64;
            runtime::engine::translate_idle_timed(self, deadline);
        }
        state().waited = true;
        unsafe { gpu_pace(ms) }
    }
    fn net_enabled(&mut self) -> bool {
        unsafe { net_enabled() != 0 }
    }
    fn net_open(&mut self, id: u32, udp: bool, host: &str, port: u16) -> bool {
        unsafe { net_open(id, udp as u32, host.as_ptr(), host.len(), port as u32) != 0 }
    }
    fn net_send(&mut self, id: u32, data: &[u8]) -> bool {
        unsafe { net_send(id, data.as_ptr(), data.len()) != 0 }
    }
    fn net_recv(&mut self, id: u32, buf: &mut [u8]) -> Option<usize> {
        let n = unsafe { net_recv(id, buf.as_mut_ptr(), buf.len()) };
        (n >= 0).then_some(n as usize)
    }
    fn net_available(&mut self, id: u32) -> u32 {
        unsafe { net_available(id) }
    }
    fn net_state(&mut self, id: u32) -> runtime::hle::winsock::NetState {
        use runtime::hle::winsock::NetState;
        match unsafe { net_state(id) } {
            0 => NetState::Connecting,
            1 => NetState::Open,
            2 => NetState::Closed,
            _ => NetState::Failed,
        }
    }
    fn net_close(&mut self, id: u32) {
        unsafe { net_close(id) }
    }
    fn soundfont(&mut self) -> Option<Vec<u8>> {
        let st = state();
        if !st.soundfont_requested {
            st.soundfont_requested = true;
            unsafe { soundfont_request() };
        }
        st.soundfont.take()
    }
}

// ---- the cpu glue module ------------------------------------------------------------------

fn build_glue(initial_pages: u64) -> Vec<u8> {
    use wasm_encoder::{
        CodeSection, ConstExpr, EntityType, ExportKind, ExportSection, Function, FunctionSection, GlobalSection, GlobalType,
        ImportSection, Instruction as I, MemorySection, MemoryType, Module, TypeSection, ValType,
    };
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32; 3], []); // 0: copy/fill
    types.ty().function([ValType::I32], [ValType::I32]); // 1: get / grow
    types.ty().function([ValType::I32; 2], []); // 2: set
    types.ty().function([], [ValType::I32]); // 3: size
    types.ty().function([ValType::I32; 2], []); // 4: vget / vset
    types.ty().function([ValType::I32; 8], [ValType::I32; 8]); // 5: a translated function
    types.ty().function([ValType::I32], []); // 6: call
    let mut imports = ImportSection::new();
    imports.import("rt", "memory", EntityType::Memory(MemoryType { minimum: 0, maximum: None, memory64: false, shared: false, page_size_log2: None }));
    imports.import(
        "rt",
        "table",
        EntityType::Table(wasm_encoder::TableType { element_type: wasm_encoder::RefType::FUNCREF, table64: false, minimum: 0, maximum: None, shared: false }),
    );
    let mut mems = MemorySection::new();
    mems.memory(MemoryType { minimum: initial_pages, maximum: Some(GUEST_MAX_PAGES), memory64: false, shared: false, page_size_log2: None });
    let mut globals = GlobalSection::new();
    for (_, _, ty) in GLOBALS {
        let init = match ty {
            ValType::V128 => ConstExpr::v128_const(0),
            _ => ConstExpr::i32_const(0),
        };
        globals.global(GlobalType { val_type: ty, mutable: true, shared: false }, &init);
    }
    let mut funcs = FunctionSection::new();
    let mut code = CodeSection::new();
    // g2r(guest, rt, len): guest memory (1) -> runtime memory (0)
    funcs.function(0);
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(1)).instruction(&I::LocalGet(0)).instruction(&I::LocalGet(2)).instruction(&I::MemoryCopy { src_mem: 1, dst_mem: 0 }).instruction(&I::End);
    code.function(&f);
    // r2g(rt, guest, len)
    funcs.function(0);
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(1)).instruction(&I::LocalGet(0)).instruction(&I::LocalGet(2)).instruction(&I::MemoryCopy { src_mem: 0, dst_mem: 1 }).instruction(&I::End);
    code.function(&f);
    // fill(guest, byte, len)
    funcs.function(0);
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(0)).instruction(&I::LocalGet(1)).instruction(&I::LocalGet(2)).instruction(&I::MemoryFill(1)).instruction(&I::End);
    code.function(&f);
    let i32_globals: Vec<u32> = GLOBALS.iter().enumerate().filter(|(_, g)| g.2 == ValType::I32).map(|(i, _)| i as u32).collect();
    let n = GLOBALS.len() as u32;
    // get(i): block ladder over every global index (v128 slots read as 0)
    funcs.function(1);
    let mut f = Function::new([]);
    f.instruction(&I::Block(wasm_encoder::BlockType::Empty));
    for _ in 0..n {
        f.instruction(&I::Block(wasm_encoder::BlockType::Empty));
    }
    f.instruction(&I::LocalGet(0)).instruction(&I::BrTable((0..n).collect::<Vec<_>>().into(), n));
    for i in 0..n {
        f.instruction(&I::End);
        if i32_globals.contains(&i) {
            f.instruction(&I::GlobalGet(i));
        } else {
            f.instruction(&I::I32Const(0));
        }
        f.instruction(&I::Return);
    }
    f.instruction(&I::End);
    f.instruction(&I::I32Const(0)).instruction(&I::End);
    code.function(&f);
    // set(i, v)
    funcs.function(2);
    let mut f = Function::new([]);
    f.instruction(&I::Block(wasm_encoder::BlockType::Empty));
    for _ in 0..n {
        f.instruction(&I::Block(wasm_encoder::BlockType::Empty));
    }
    f.instruction(&I::LocalGet(0)).instruction(&I::BrTable((0..n).collect::<Vec<_>>().into(), n));
    for i in 0..n {
        f.instruction(&I::End);
        if i32_globals.contains(&i) {
            f.instruction(&I::LocalGet(1)).instruction(&I::GlobalSet(i));
        }
        f.instruction(&I::Return);
    }
    f.instruction(&I::End).instruction(&I::End);
    code.function(&f);
    // size() / grow(n) of guest memory
    funcs.function(3);
    let mut f = Function::new([]);
    f.instruction(&I::MemorySize(1)).instruction(&I::End);
    code.function(&f);
    funcs.function(1);
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(0)).instruction(&I::MemoryGrow(1)).instruction(&I::End);
    code.function(&f);
    // vget(i, ptr) / vset(i, ptr): XMM register i to / from runtime memory
    let xmm0 = abi::Global::Xmm0 as u32;
    let mem0 = wasm_encoder::MemArg { offset: 0, align: 0, memory_index: 0 };
    for store in [true, false] {
        funcs.function(4);
        let mut f = Function::new([]);
        f.instruction(&I::Block(wasm_encoder::BlockType::Empty));
        for _ in 0..8 {
            f.instruction(&I::Block(wasm_encoder::BlockType::Empty));
        }
        f.instruction(&I::LocalGet(0)).instruction(&I::BrTable((0..8).collect::<Vec<_>>().into(), 8));
        for k in 0..8 {
            f.instruction(&I::End);
            if store {
                f.instruction(&I::LocalGet(1)).instruction(&I::GlobalGet(xmm0 + k)).instruction(&I::V128Store(mem0));
            } else {
                f.instruction(&I::LocalGet(1)).instruction(&I::V128Load(mem0)).instruction(&I::GlobalSet(xmm0 + k));
            }
            f.instruction(&I::Return);
        }
        f.instruction(&I::End).instruction(&I::End);
        code.function(&f);
    }
    // call(idx): registers from the globals, through the function, back
    funcs.function(6);
    let mut f = Function::new([]);
    for r in 0..8 {
        f.instruction(&I::GlobalGet(r));
    }
    f.instruction(&I::LocalGet(0)).instruction(&I::CallIndirect { type_index: 5, table_index: 0 });
    for r in (0..8).rev() {
        f.instruction(&I::GlobalSet(r));
    }
    f.instruction(&I::End);
    code.function(&f);

    // gcopy(dst, src, len): within guest memory
    funcs.function(0);
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(0)).instruction(&I::LocalGet(1)).instruction(&I::LocalGet(2)).instruction(&I::MemoryCopy { src_mem: 1, dst_mem: 1 }).instruction(&I::End);
    code.function(&f);

    // rget(rt): the registers of `Regs` into runtime memory; rset(rt): back
    let mem0 = wasm_encoder::MemArg { offset: 0, align: 2, memory_index: 0 };
    funcs.function(6);
    let mut f = Function::new([]);
    for (k, g) in REG_GLOBALS.iter().enumerate() {
        f.instruction(&I::LocalGet(0)).instruction(&I::GlobalGet(*g as u32)).instruction(&I::I32Store(wasm_encoder::MemArg { offset: 4 * k as u64, ..mem0 }));
    }
    f.instruction(&I::End);
    code.function(&f);
    funcs.function(6);
    let mut f = Function::new([]);
    for (k, g) in REG_GLOBALS.iter().enumerate() {
        f.instruction(&I::LocalGet(0)).instruction(&I::I32Load(wasm_encoder::MemArg { offset: 4 * k as u64, ..mem0 })).instruction(&I::GlobalSet(*g as u32));
    }
    f.instruction(&I::End);
    code.function(&f);

    let mut exports = ExportSection::new();
    exports.export(abi::MEMORY_NAME, ExportKind::Memory, 1);
    for (i, (_, name, _)) in GLOBALS.iter().enumerate() {
        exports.export(name, ExportKind::Global, i as u32);
    }
    for (i, name) in GLUE_FUNCS.iter().enumerate() {
        exports.export(name, ExportKind::Func, i as u32);
    }
    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&mems);
    m.section(&globals);
    m.section(&exports);
    m.section(&code);
    m.finish()
}

static mut GLUE_BYTES: Vec<u8> = Vec::new();

// ---- exports ---------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v = vec![0u8; n.max(1)];
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(p: *mut u8, n: usize) {
    drop(unsafe { Vec::from_raw_parts(p, n.max(1), n.max(1)) });
}

/// Builds the cpu glue module; returns its length (bytes at `glue_ptr()`).
#[unsafe(no_mangle)]
#[allow(static_mut_refs)]
pub extern "C" fn glue_build(initial_pages: u32) -> usize {
    unsafe {
        GLUE_BYTES = build_glue(initial_pages as u64);
        GLUE_BYTES.len()
    }
}

#[unsafe(no_mangle)]
#[allow(static_mut_refs)]
pub extern "C" fn glue_ptr() -> *const u8 {
    unsafe { GLUE_BYTES.as_ptr() }
}

/// The glue accessors are at table slots `base..base + 7` in `GLUE_FUNCS` order.
#[unsafe(no_mangle)]
pub extern "C" fn glue_install(base: u32) {
    let f = |i: u32| (base + i) as usize;
    unsafe {
        state().glue = Some(Glue {
            g2r: std::mem::transmute(f(0)),
            r2g: std::mem::transmute(f(1)),
            fill: std::mem::transmute(f(2)),
            get: std::mem::transmute(f(3)),
            set: std::mem::transmute(f(4)),
            size: std::mem::transmute(f(5)),
            grow: std::mem::transmute(f(6)),
            vget: std::mem::transmute(f(7)),
            vset: std::mem::transmute(f(8)),
            call: std::mem::transmute(f(9)),
            gcopy: std::mem::transmute(f(10)),
            rget: std::mem::transmute(f(11)),
            rset: std::mem::transmute(f(12)),
        });
    }
}

/// Adds a game file (`path` relative to the game directory, `\`-separated) with its
/// last-write time in Unix milliseconds (0 if unknown).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_add(path: *const u8, path_len: usize, data: *const u8, data_len: usize, mtime_ms: f64) {
    let path = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(path, path_len) }).into_owned();
    let data = unsafe { std::slice::from_raw_parts(data, data_len) }.to_vec();
    let ms = mtime_ms.max(0.0) as u64;
    let filetime = if ms == 0 { 0 } else { (ms / 1000 + 11_644_473_600) * 10_000_000 + (ms % 1000) * 10_000 };
    state().fs.get_or_insert_with(MemFs::new).insert_with_time(&path, data, filetime);
}

/// Creates the process and loads the executable. Returns 0 on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init(exe: *const u8, exe_len: usize, frames: u32, trace: u32, epoch_unix: f64, nop_index: u32, render: u32) -> i32 {
    let exe = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(exe, exe_len) }).into_owned();
    let fs = state().fs.take().unwrap_or_default();
    let exe_path = format!("C:\\game\\{exe}");
    let mut p = Process::new(Box::new(fs), &exe_path, &format!("\"{exe_path}\""));
    p.frame_limit = if frames == 0 { u64::MAX } else { frames as u64 };
    p.trace = trace != 0;
    p.epoch_unix = epoch_unix as u64;
    if render != 0 {
        p.gpu = Box::new(WebGpu);
        p.audio = Box::new(WebAudio);
        p.saves = Box::new(WebSaves);
    } else {
        p.gpu = Box::new(FrameGpu);
    }
    let mut e = Engine::new(p);
    e.trace = trace != 0;
    e.nop_index = nop_index;
    state().engine = Some(e);
    let mut m = Web;
    match engine::start(&mut m, &exe) {
        Ok(s) => {
            state().started = Some(s);
            0
        }
        Err(e) => {
            log(&format!("start failed: {e}"));
            1
        }
    }
}

/// Runs the process until it exits or reaches the frame limit; prints a summary.
/// Returns 0 when it ended normally.
#[unsafe(no_mangle)]
pub extern "C" fn run() -> i32 {
    let mut m = Web;
    let Some(started) = state().started.take() else {
        log("run: not started");
        return 2;
    };
    let t0 = m.now_us();
    let r = engine::run_process(&mut m, &started);
    let dt = (m.now_us() - t0) as f64 / 1e6;
    let (hash, chunks) = engine::state_hash(&mut m, (GUEST_MAX_PAGES * 65536 - 65536) as u32);
    let e = m.engine_ref();
    let p = e.process();
    let mut lines = vec![
        format!("---- run finished in {dt:.2}s ----"),
        match &r {
            Ok(()) => "ended normally".to_string(),
            Err(err) => format!("stopped: {err}"),
        },
        format!("state hash: {hash:016x} ({chunks} non-zero 64K chunks)"),
        format!(
            "frames presented: {}  api calls: {}  dispatches: {}  resolves: {}  translated functions: {} ({:.2}s; {} ahead in {} waits, {:.2}s, ~{} µs + {} ns/byte; profile {} left)  virtual time: {}ms",
            p.frames_presented,
            p.api_calls,
            e.dispatches,
            e.resolves,
            e.translated,
            e.translate_us as f64 / 1e6,
            e.idle_done,
            e.idle_calls,
            e.idle_us as f64 / 1e6,
            e.idle_fixed_ns / 1000,
            e.idle_ns_per_byte,
            e.profile.len(),
            p.now_ms()
        ),
    ];
    let presents: Vec<&String> = p.log.iter().filter(|l| l.starts_with("Present #")).collect();
    lines.extend(presents.iter().skip(presents.len().saturating_sub(2)).map(|s| s.to_string()));
    // What the guest printed and what the HLE flagged, as the harness shows them.
    let mut seen = std::collections::HashSet::new();
    let notable = p.log.iter().filter(|l| {
        ["stdout:", "printf:", "puts:", "[", "exception", "MessageBox", "MCI", "DirectMusic"].iter().any(|k| l.starts_with(k))
            || l.contains("not implemented")
            || l.contains("not registered")
    });
    lines.extend(notable.filter(|l| seen.insert(l.as_str())).take(200).map(|s| s.to_string()));
    for l in lines {
        log(&l);
    }
    if r.is_ok() { 0 } else { 1 }
}

/// Runtime statistics so far, for the page: host time (µs) spent on 0 the other
/// guest threads, 1 translating ahead, 2 translating in all; 3 api calls; 4 dispatches.
#[unsafe(no_mangle)]
pub extern "C" fn stat_us(which: u32) -> f64 {
    state().engine.as_ref().map_or(0.0, |e| match which {
        0 => e.thread_us,
        1 => e.idle_us,
        2 => e.translate_us,
        3 => e.api_calls,
        _ => e.dispatches,
    } as f64)
}

/// Queues a function earlier sessions needed, with its code hash and the frame it was
/// first needed in; entries go in last needed first (see `Engine::profile`).
#[unsafe(no_mangle)]
pub extern "C" fn profile_push(addr: u32, hash: u32, frame: u32) {
    engine::profile_push(&mut Web, addr, hash, frame);
}

/// Registers the loaded modules (discovery) and returns how many there are, for
/// `module_at` (profiles shared between games with the same runner).
#[unsafe(no_mangle)]
pub extern "C" fn module_count() -> u32 {
    engine::sync_modules(&mut Web);
    state().engine.as_ref().map_or(0, |e| e.mods.len() as u32)
}

/// Module `i`: its base (`which` 0), size (1) or code id (2, `ModInfo::code_id`).
#[unsafe(no_mangle)]
pub extern "C" fn module_at(i: u32, which: u32) -> u32 {
    state().engine.as_ref().and_then(|e| e.mods.get(i as usize)).map_or(0, |md| [md.base, md.size, md.code_id][which.min(2) as usize])
}

/// How many functions this session has translated when they ran (`Engine::demand`).
#[unsafe(no_mangle)]
pub extern "C" fn demand_len() -> u32 {
    state().engine.as_ref().map_or(0, |e| e.demand.len() as u32)
}

/// Entry `i` of `Engine::demand`: its address (`which` 0), code hash (1) or frame (2).
#[unsafe(no_mangle)]
pub extern "C" fn demand_at(i: u32, which: u32) -> u32 {
    state().engine.as_ref().and_then(|e| e.demand.get(i as usize)).map_or(0, |&(a, h, f)| [a, h, f][which.min(2) as usize])
}

/// Queues an input event of a recorded session (`kind` 0 key: a = vk, b = scan,
/// c = down; 1 mouse: x, y, buttons), delivered at the poll of `frame` and `clock_us`.
#[unsafe(no_mangle)]
pub extern "C" fn replay_push(kind: u32, frame: f64, clock_us: f64, a: i32, b: i32, c: i32) {
    let ev = match kind {
        0 => runtime::InputEvent::Key { vk: a as u8, scan: b as u16, down: c != 0 },
        _ => runtime::InputEvent::Mouse { x: a, y: b, buttons: c as u32 },
    };
    state().replay.push_back((frame as u64, clock_us as u64, ev));
}

/// Prints each input event as the guest takes it (`input <frame> <clock> ...`) for the
/// page's monitor mode: the session can then be replayed (`harness run --replay`).
#[unsafe(no_mangle)]
pub extern "C" fn set_monitor(on: u32) {
    if let Some(e) = state().engine.as_mut() {
        e.monitor = on != 0;
    }
}

/// A key event from the page (Windows virtual-key code, scan code with 0x100 for
/// extended keys).
#[unsafe(no_mangle)]
pub extern "C" fn input_key(vk: u32, scan: u32, down: u32) {
    state().input.push(runtime::InputEvent::Key { vk: vk as u8, scan: scan as u16, down: down != 0 });
}

/// Mouse position in game-window client pixels and button state (1 left, 2 right, 4 middle).
#[unsafe(no_mangle)]
pub extern "C" fn input_mouse(x: i32, y: i32, buttons: u32) {
    state().input.push(runtime::InputEvent::Mouse { x, y, buttons });
}

/// The MIDI SoundFont the page fetched (bytes at `ptr`, from `alloc`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn soundfont_add(ptr: *mut u8, len: usize) {
    let bytes = unsafe { Vec::from_raw_parts(ptr, len, len) };
    state().soundfont = Some(bytes);
}

/// A game controller's state in XInput terms (see `runtime::InputEvent::Pad`):
/// `triggers` = left | right << 8.
#[unsafe(no_mangle)]
pub extern "C" fn input_pad(index: u32, connected: u32, buttons: u32, triggers: u32, lx: i32, ly: i32, rx: i32, ry: i32) {
    state().input.push(runtime::InputEvent::Pad {
        index: index as u8,
        connected: connected != 0,
        buttons: buttons as u16,
        triggers: [triggers as u8, (triggers >> 8) as u8],
        thumbs: [lx as i16, ly as i16, rx as i16, ry as i16],
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn rt_resolve(addr: i32) -> i32 {
    engine::rt_resolve(&mut Web, addr as u32) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn rt_trap(kind: i32, arg: i32) {
    engine::rt_trap(&mut Web, kind, arg as u32)
}

#[unsafe(no_mangle)]
pub extern "C" fn rt_fpu(code: i32, a: i32, b: i32, ip: i32, fop: i32) -> i32 {
    engine::rt_fpu(&mut Web, code, a as u32, b as u32, ip as u32, fop as u32)
}

#[unsafe(no_mangle)]
pub extern "C" fn rt_cpuid() {
    engine::rt_cpuid(&mut Web)
}

#[unsafe(no_mangle)]
pub extern "C" fn rt_rdtsc() {
    engine::rt_rdtsc(&mut Web)
}

/// The no-op module's bytes (JS instantiates it first; its function's slot goes to `init`).
#[unsafe(no_mangle)]
#[allow(static_mut_refs)]
pub extern "C" fn nop_build() -> usize {
    unsafe {
        GLUE_BYTES = engine::nop_module();
        GLUE_BYTES.len()
    }
}

/// fsin's and fcos's values for the x87 fast-path module (its `rt.sin`, `rt.cos`).
#[unsafe(no_mangle)]
pub extern "C" fn x87_sin(x: f64) -> f64 {
    fpu::sin(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn x87_cos(x: f64) -> f64 {
    fpu::cos(x)
}

/// The x87 fast-path module's bytes (JS instantiates it once; translated modules import it).
#[unsafe(no_mangle)]
#[allow(static_mut_refs)]
pub extern "C" fn x87_build() -> usize {
    unsafe {
        GLUE_BYTES = recomp::x87::module(false);
        GLUE_BYTES.len()
    }
}
