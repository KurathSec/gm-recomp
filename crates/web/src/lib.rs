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
}

/// Export order of the glue accessors; JS places them in the table in this order.
const GLUE_FUNCS: [&str; 9] = ["g2r", "r2g", "fill", "get", "set", "size", "grow", "vget", "vset"];

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
}

static mut STATE: State = State { input: Vec::new(), engine: None, glue: None, fs: None, started: None, guest_bytes: 0, soundfont_requested: false, soundfont: None };

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
        // Table slots hold translated functions of type () -> ().
        let f: extern "C" fn() = unsafe { std::mem::transmute(idx as usize) };
        f();
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
    fn take_input(&mut self, _frame: u64) -> Vec<runtime::InputEvent> {
        Web::take_input()
    }
    fn pace(&mut self, clock_us: u64) {
        unsafe { gpu_pace(clock_us as f64 / 1000.0) }
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

impl Web {
    fn take_input() -> Vec<runtime::InputEvent> {
        std::mem::take(&mut state().input)
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
    let mut imports = ImportSection::new();
    imports.import("rt", "memory", EntityType::Memory(MemoryType { minimum: 0, maximum: None, memory64: false, shared: false, page_size_log2: None }));
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
            "frames presented: {}  api calls: {}  dispatches: {}  resolves: {}  translated functions: {} ({:.1}s)  virtual time: {}ms",
            p.frames_presented,
            p.api_calls,
            e.dispatches,
            e.resolves,
            e.translated,
            e.translate_us as f64 / 1e6,
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

/// The x87 fast-path module's bytes (JS instantiates it once; translated modules import it).
#[unsafe(no_mangle)]
#[allow(static_mut_refs)]
pub extern "C" fn x87_build() -> usize {
    unsafe {
        GLUE_BYTES = recomp::x87::module(false);
        GLUE_BYTES.len()
    }
}
