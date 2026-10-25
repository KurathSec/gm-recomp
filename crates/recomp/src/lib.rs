//! x86 to WebAssembly static recompiler: 32-bit code, and x86-64 code in modules of
//! its own (`Options::x64`, docs/x64.md).

pub mod abi;
pub mod cfg;
pub mod fasthash;
pub mod flags;
pub mod layout;
pub mod lift;
pub mod lift64;
pub mod sse;
pub mod x87;

use abi::{GLOBALS, RT_FUNCS, types};
use cfg::{Cfg, FuncInfo, GuestMem};
use crate::fasthash::HashMap;
use std::collections::BTreeMap;
use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, FunctionSection, GlobalType, ImportSection, MemoryType, Module,
    RefType, TableType, TypeSection, ValType,
};

pub use cfg::GuestMem as Memory;

#[derive(Default)]
pub struct Options {
    /// Declare the imported guest memory as shared (browser threads) or not (harness).
    pub shared_memory: bool,
    /// Function entries that report to the runtime when reached (debugging).
    pub probes: Vec<u32>,
    /// Send every x87 instruction through `rt.fpu` (to check the inline paths).
    pub no_x87_inline: bool,
    /// Probe `abi::RESOLVE_CACHE` before calling `rt.resolve` (needs the cache's
    /// memory; the CPU tests run without it).
    pub resolve_cache: bool,
    /// Count events in `abi::COUNTERS` (debugging).
    pub count: bool,
    /// With `count`: `abi::ENTRY_COUNTS` counts blocks (by their first instruction's
    /// address) instead of function entries.
    pub count_blocks: bool,
    /// Do not inline small leaf callees (checking the inlining).
    pub no_inline: bool,
    /// Translate x86-64 code: every function of the module is 64-bit (`types::FUNC64`),
    /// decoded at `FuncInfo::high << 32 | address`.
    pub x64: bool,
}

/// A function body larger than this is emitted again without inline x87 (the limit
/// browsers enforce is 7,654,321 bytes).
const MAX_FUNCTION_BYTES: usize = 7_000_000;

pub struct Translator<'m> {
    mem: &'m dyn GuestMem,
    opts: Options,
    cfgs: BTreeMap<u32, Cfg>,
    pub unimplemented: Vec<(u32, String)>,
    /// Callees considered for inlining (hand it to the next translator: `take_cache`).
    cache: cfg::InlineCache,
}

pub struct Output {
    pub wasm: Vec<u8>,
    /// Entry address -> export name, in function-index order.
    pub functions: Vec<(u32, String)>,
    /// Instructions that were emitted as `UNIMPLEMENTED` traps: (address, disassembly).
    pub unimplemented: Vec<(u32, String)>,
    /// Entry -> the address ranges of the code each function was translated from
    /// (inlined callees' included), merged.
    pub ranges: Vec<(u32, Vec<(u32, u32)>)>,
}

/// The code ranges of a function's blocks, merged.
fn code_ranges(cfg: &Cfg) -> Vec<(u32, u32)> {
    let mut r: Vec<(u32, u32)> = cfg
        .blocks
        .values()
        .filter_map(|b| Some((b.instrs.first()?.ip() as u32, b.instrs.last()?.next_ip() as u32)))
        .collect();
    r.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(r.len());
    for (s, e) in r {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// A module of stand-ins for 32-bit functions whose translations were dropped (their
/// code changed): each looks its address up again (the resolve cache, else `rt.resolve`,
/// which translates it anew) and calls that, so the table slot of the old translation,
/// which callers keep, leads to the new one.
pub fn redirects(entries: &[u32], shared_memory: bool) -> Output {
    use wasm_encoder::{Function, Instruction as W, MemArg};
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32; 8], [ValType::I32; 8]); // FUNC
    types.ty().function([ValType::I32], [ValType::I32]); // RESOLVE
    let mut imports = ImportSection::new();
    imports.import(
        abi::CPU_MODULE,
        abi::MEMORY_NAME,
        EntityType::Memory(MemoryType { minimum: 0, maximum: if shared_memory { Some(65536) } else { None }, memory64: false, shared: shared_memory, page_size_log2: None }),
    );
    imports.import(
        abi::CPU_MODULE,
        abi::TABLE_NAME,
        EntityType::Table(TableType { element_type: RefType::FUNCREF, table64: false, minimum: 0, maximum: None, shared: false }),
    );
    // eax..edi, the registers `rt.resolve` may look at
    for &(_, name, ty) in &GLOBALS[..8] {
        imports.import(abi::CPU_MODULE, name, EntityType::Global(GlobalType { val_type: ty, mutable: true, shared: false }));
    }
    let resolve = RT_FUNCS.iter().find(|(f, _)| matches!(f, abi::RtFunc::Resolve)).map(|(_, n)| *n).expect("resolve");
    imports.import(abi::RT_MODULE, resolve, EntityType::Function(1));
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut out_funcs = Vec::new();
    let at = |off: u32| MemArg { offset: off as u64, align: 2, memory_index: 0 };
    for (i, &entry) in entries.iter().enumerate() {
        functions.function(0);
        let name = abi::export_name(entry);
        exports.export(&name, ExportKind::Func, 1 + i as u32);
        let mut f = Function::new([(1, ValType::I32)]);
        let slot = abi::resolve_slot(entry) as i32;
        f.instruction(&W::I32Const(slot));
        f.instruction(&W::I32Load(at(abi::RESOLVE_CACHE)));
        f.instruction(&W::I32Const(entry as i32));
        f.instruction(&W::I32Eq);
        f.instruction(&W::If(wasm_encoder::BlockType::Result(ValType::I32)));
        f.instruction(&W::I32Const(slot));
        f.instruction(&W::I32Load(at(abi::RESOLVE_CACHE + 4)));
        f.instruction(&W::Else);
        for r in 0..8 {
            f.instruction(&W::LocalGet(r));
            f.instruction(&W::GlobalSet(r));
        }
        f.instruction(&W::I32Const(entry as i32));
        f.instruction(&W::Call(0));
        for r in 0..8 {
            f.instruction(&W::GlobalGet(r));
            f.instruction(&W::LocalSet(r));
        }
        f.instruction(&W::End);
        f.instruction(&W::LocalSet(8));
        for r in 0..8 {
            f.instruction(&W::LocalGet(r));
        }
        f.instruction(&W::LocalGet(8));
        f.instruction(&W::CallIndirect { type_index: 0, table_index: 0 });
        f.instruction(&W::End);
        code.function(&f);
        out_funcs.push((entry, name));
    }
    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);
    Output { wasm: module.finish(), functions: out_funcs, unimplemented: Vec::new(), ranges: Vec::new() }
}

/// C runtime functions done in wasm instead of through the runtime (an extension that
/// calls `abs` for every pixel spent most of its time getting there and back): each
/// takes its cdecl arguments from the guest stack, leaves its result in eax and returns
/// as an API call does (eip the return address, esp past it). A range that is not all
/// in memory is left alone. Returns the module and (API name, export name) pairs.
pub fn intrinsics(shared_memory: bool) -> (Vec<u8>, Vec<(&'static str, String)>) {
    use wasm_encoder::{BlockType, Function, Instruction as W, MemArg};
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32; 8], [ValType::I32; 8]); // FUNC
    let mut imports = ImportSection::new();
    imports.import(
        abi::CPU_MODULE,
        abi::MEMORY_NAME,
        EntityType::Memory(MemoryType { minimum: 0, maximum: if shared_memory { Some(65536) } else { None }, memory64: false, shared: shared_memory, page_size_log2: None }),
    );
    let eip = GLOBALS.iter().find(|g| matches!(g.0, abi::Global::Eip)).expect("eip");
    imports.import(abi::CPU_MODULE, eip.1, EntityType::Global(GlobalType { val_type: ValType::I32, mutable: true, shared: false }));
    let at = |off: u32| MemArg { offset: off as u64, align: 0, memory_index: 0 };
    const ESP: u32 = 4;
    const A: u32 = 8; // first argument, then B, C
    const B: u32 = 9;
    const C: u32 = 10;
    // (args into A.., the body, then eax = local `ret`, the return)
    let arg = |f: &mut Function, k: u32, to: u32| {
        f.instruction(&W::LocalGet(ESP));
        f.instruction(&W::I32Load(at(4 + 4 * k)));
        f.instruction(&W::LocalSet(to));
    };
    // whether [a, a + n) lies in memory (i64: no wrapping)
    let in_memory = |f: &mut Function, a: u32, n: u32| {
        f.instruction(&W::LocalGet(a));
        f.instruction(&W::I64ExtendI32U);
        f.instruction(&W::LocalGet(n));
        f.instruction(&W::I64ExtendI32U);
        f.instruction(&W::I64Add);
        f.instruction(&W::MemorySize(0));
        f.instruction(&W::I64ExtendI32U);
        f.instruction(&W::I64Const(16));
        f.instruction(&W::I64Shl);
        f.instruction(&W::I64LeU);
    };
    let finish = |f: &mut Function, ret: u32| {
        f.instruction(&W::LocalGet(ret));
        f.instruction(&W::LocalSet(0));
        f.instruction(&W::LocalGet(ESP));
        f.instruction(&W::I32Load(at(0)));
        f.instruction(&W::GlobalSet(0));
        f.instruction(&W::LocalGet(ESP));
        f.instruction(&W::I32Const(4));
        f.instruction(&W::I32Add);
        f.instruction(&W::LocalSet(ESP));
        for r in 0..8 {
            f.instruction(&W::LocalGet(r));
        }
        f.instruction(&W::End);
    };
    let mut bodies: Vec<(&'static str, Function)> = Vec::new();
    for name in ["abs", "labs"] {
        let mut f = Function::new([(4, ValType::I32)]);
        arg(&mut f, 0, A);
        // (x ^ x >> 31) - (x >> 31)
        f.instruction(&W::LocalGet(A));
        f.instruction(&W::I32Const(31));
        f.instruction(&W::I32ShrS);
        f.instruction(&W::LocalSet(B));
        f.instruction(&W::LocalGet(A));
        f.instruction(&W::LocalGet(B));
        f.instruction(&W::I32Xor);
        f.instruction(&W::LocalGet(B));
        f.instruction(&W::I32Sub);
        f.instruction(&W::LocalSet(C));
        finish(&mut f, C);
        bodies.push((name, f));
    }
    {
        // memset(d, v, n) -> d
        let mut f = Function::new([(4, ValType::I32)]);
        arg(&mut f, 0, A);
        arg(&mut f, 1, B);
        arg(&mut f, 2, C);
        in_memory(&mut f, A, C);
        f.instruction(&W::If(BlockType::Empty));
        f.instruction(&W::LocalGet(A));
        f.instruction(&W::LocalGet(B));
        f.instruction(&W::LocalGet(C));
        f.instruction(&W::MemoryFill(0));
        f.instruction(&W::End);
        finish(&mut f, A);
        bodies.push(("memset", f));
    }
    for name in ["memcpy", "memmove"] {
        // (d, s, n) -> d
        let mut f = Function::new([(4, ValType::I32)]);
        arg(&mut f, 0, A);
        arg(&mut f, 1, B);
        arg(&mut f, 2, C);
        in_memory(&mut f, A, C);
        in_memory(&mut f, B, C);
        f.instruction(&W::I32And);
        f.instruction(&W::If(BlockType::Empty));
        f.instruction(&W::LocalGet(A));
        f.instruction(&W::LocalGet(B));
        f.instruction(&W::LocalGet(C));
        f.instruction(&W::MemoryCopy { src_mem: 0, dst_mem: 0 });
        f.instruction(&W::End);
        finish(&mut f, A);
        bodies.push((name, f));
    }
    {
        // strlen(s) -> the length (to the end of memory at most)
        let mut f = Function::new([(4, ValType::I32)]);
        arg(&mut f, 0, A);
        f.instruction(&W::I32Const(0));
        f.instruction(&W::LocalSet(C));
        f.instruction(&W::MemorySize(0));
        f.instruction(&W::I32Const(16));
        f.instruction(&W::I32Shl);
        f.instruction(&W::LocalSet(B)); // (the memory's end; 0 for 4 GB, never reached)
        f.instruction(&W::Block(BlockType::Empty));
        f.instruction(&W::Loop(BlockType::Empty));
        f.instruction(&W::LocalGet(A));
        f.instruction(&W::LocalGet(C));
        f.instruction(&W::I32Add);
        f.instruction(&W::LocalTee(A + 3));
        f.instruction(&W::LocalGet(B));
        f.instruction(&W::I32GeU);
        f.instruction(&W::LocalGet(B));
        f.instruction(&W::I32Const(0));
        f.instruction(&W::I32Ne);
        f.instruction(&W::I32And);
        f.instruction(&W::BrIf(1));
        f.instruction(&W::LocalGet(A + 3));
        f.instruction(&W::I32Load8U(at(0)));
        f.instruction(&W::I32Eqz);
        f.instruction(&W::BrIf(1));
        f.instruction(&W::LocalGet(C));
        f.instruction(&W::I32Const(1));
        f.instruction(&W::I32Add);
        f.instruction(&W::LocalSet(C));
        f.instruction(&W::Br(0));
        f.instruction(&W::End);
        f.instruction(&W::End);
        finish(&mut f, C);
        bodies.push(("strlen", f));
    }
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut names = Vec::new();
    for (i, (name, f)) in bodies.iter().enumerate() {
        functions.function(0);
        let export = format!("crt_{name}");
        exports.export(&export, ExportKind::Func, i as u32);
        code.function(f);
        names.push((*name, export));
    }
    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&exports);
    module.section(&code);
    (module.finish(), names)
}

/// Delphi's System.Move (`cfg::is_delphi_move`) as one `memory.copy`: memmove(edx, eax,
/// ecx) for a count above zero, then its `ret` (eip the return address, esp past it).
fn delphi_move() -> wasm_encoder::Function {
    use wasm_encoder::{BlockType, Function, Instruction as W, MemArg};
    const EAX: u32 = 0;
    const ECX: u32 = 1;
    const EDX: u32 = 2;
    const ESP: u32 = 4;
    let mut f = Function::new([]);
    f.instruction(&W::LocalGet(ECX));
    f.instruction(&W::I32Const(0));
    f.instruction(&W::I32GtS);
    f.instruction(&W::LocalGet(EAX));
    f.instruction(&W::LocalGet(EDX));
    f.instruction(&W::I32Ne);
    f.instruction(&W::I32And);
    f.instruction(&W::If(BlockType::Empty));
    f.instruction(&W::LocalGet(EDX));
    f.instruction(&W::LocalGet(EAX));
    f.instruction(&W::LocalGet(ECX));
    f.instruction(&W::MemoryCopy { src_mem: 0, dst_mem: 0 });
    f.instruction(&W::End);
    f.instruction(&W::LocalGet(ESP));
    f.instruction(&W::I32Load(MemArg { offset: 0, align: 0, memory_index: 0 }));
    f.instruction(&W::GlobalSet(abi::Global::Eip as u32));
    f.instruction(&W::LocalGet(ESP));
    f.instruction(&W::I32Const(4));
    f.instruction(&W::I32Add);
    f.instruction(&W::LocalSet(ESP));
    for r in 0..8 {
        f.instruction(&W::LocalGet(r));
    }
    f.instruction(&W::End);
    f
}

impl<'m> Translator<'m> {
    pub fn new(mem: &'m dyn GuestMem, opts: Options) -> Self {
        Translator { mem, opts, cfgs: BTreeMap::new(), unimplemented: Vec::new(), cache: Default::default() }
    }

    /// Starts from the inlining cache of an earlier translator.
    pub fn with_cache(mut self, cache: HashMap<(u32, u8), Option<std::rc::Rc<Cfg>>>) -> Self {
        self.cache = std::cell::RefCell::new(cache);
        self
    }

    /// The inlining cache, for the next translator.
    pub fn take_cache(&mut self) -> HashMap<(u32, u8), Option<std::rc::Rc<Cfg>>> {
        std::mem::take(&mut *self.cache.borrow_mut())
    }

    pub fn add_function(&mut self, entry: u32, info: &FuncInfo) -> Result<(), String> {
        if self.cfgs.contains_key(&entry) {
            return Ok(());
        }
        let bits = if self.opts.x64 { cfg::Bits::B64 { high: info.high } } else { cfg::Bits::B32 };
        let cfg = cfg::build(self.mem, entry, info, !self.opts.no_inline, &self.cache, bits)?;
        self.cfgs.insert(entry, cfg);
        Ok(())
    }

    /// Entries of the functions added so far.
    pub fn entries(&self) -> impl Iterator<Item = u32> + '_ {
        self.cfgs.keys().copied()
    }

    /// A hash of everything the function at `entry` (added) was translated from: the
    /// address and bytes of each instruction in it, inlined callees' included, and its
    /// jump tables' targets. Code that changed since gives another; None if some of it
    /// cannot be read.
    pub fn code_hash(&self, entry: u32) -> Option<u32> {
        let cfg = self.cfgs.get(&entry)?;
        let mut h = 0x811c_9dc5u32;
        let mut mix = |b: &[u8]| {
            for &x in b {
                h = (h ^ x as u32).wrapping_mul(0x0100_0193);
            }
        };
        for b in cfg.blocks.values() {
            for i in &b.instrs {
                let mut buf = [0u8; 16];
                let n = i.len().min(16);
                if !self.mem.read(i.ip() as u32, &mut buf[..n]) {
                    return None;
                }
                mix(&(i.ip() as u32).to_le_bytes());
                mix(&buf[..n]);
            }
            if let cfg::Term::Table(targets) = &b.term {
                for t in targets {
                    mix(&t.to_le_bytes());
                }
            }
        }
        Some(h)
    }

    /// Drops a function added earlier (before `finish`).
    pub fn remove_function(&mut self, entry: u32) {
        self.cfgs.remove(&entry);
    }

    /// Functions the one at `entry` (added) calls or jumps to directly.
    pub fn direct_targets(&self, entry: u32) -> Vec<u32> {
        let Some(cfg) = self.cfgs.get(&entry) else { return Vec::new() };
        let mut out = Vec::new();
        for b in cfg.blocks.values() {
            for i in &b.instrs {
                if i.flow_control() == iced_x86::FlowControl::Call && cfg::is_near(i) {
                    out.push(i.near_branch_target() as u32);
                }
            }
            if let cfg::Term::TailCall(t) = b.term {
                out.push(t);
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.cfgs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cfgs.is_empty()
    }

    pub fn finish(mut self) -> Output {
        // Imported functions: the runtime services, then every x87 fast path (64-bit
        // code has no x87: none there).
        let x64 = self.opts.x64;
        let n_rt = (RT_FUNCS.len() + if x64 { 0 } else { x87::FORMS.len() }) as u32;
        let func_index: HashMap<u32, u32> = self.cfgs.keys().enumerate().map(|(i, &e)| (e, n_rt + i as u32)).collect();

        let mut types = TypeSection::new();
        types.ty().function([ValType::I32; 8], [ValType::I32; 8]); // FUNC
        types.ty().function([ValType::I32], [ValType::I32]); // RESOLVE
        types.ty().function([ValType::I32, ValType::I32], []); // TRAP
        types.ty().function([ValType::I32; 5], [ValType::I32]); // FPU
        debug_assert_eq!(types::FPU, 3);
        let (p, r) = x87::form_type();
        types.ty().function(p, r); // X87
        types.ty().function([], []); // VOID
        if x64 {
            types.ty().function([ValType::I64; 16], [ValType::I64; 16]); // FUNC64
        }

        let mut imports = ImportSection::new();
        imports.import(
            abi::CPU_MODULE,
            abi::MEMORY_NAME,
            EntityType::Memory(MemoryType {
                minimum: 0,
                maximum: if self.opts.shared_memory { Some(65536) } else { None },
                memory64: false,
                shared: self.opts.shared_memory,
                page_size_log2: None,
            }),
        );
        imports.import(
            abi::CPU_MODULE,
            abi::TABLE_NAME,
            EntityType::Table(TableType { element_type: RefType::FUNCREF, table64: false, minimum: 0, maximum: None, shared: false }),
        );
        let globals = if x64 { &GLOBALS[..] } else { &GLOBALS[..abi::GLOBALS32] };
        for &(_, name, ty) in globals {
            imports.import(abi::CPU_MODULE, name, EntityType::Global(GlobalType { val_type: ty, mutable: true, shared: false }));
        }
        for (f, name) in RT_FUNCS {
            let ty = match f {
                abi::RtFunc::Resolve => types::RESOLVE,
                abi::RtFunc::Trap => types::TRAP,
                abi::RtFunc::Fpu => types::FPU,
                _ => types::VOID,
            };
            imports.import(abi::RT_MODULE, name, EntityType::Function(ty));
        }
        for k in 0..if x64 { 0 } else { x87::FORMS.len() } {
            imports.import(x87::MODULE, &x87::export_name(k), EntityType::Function(types::X87));
        }

        let mut functions = FunctionSection::new();
        let mut exports = ExportSection::new();
        let mut code = CodeSection::new();
        let mut out_funcs = Vec::new();
        let ranges: Vec<(u32, Vec<(u32, u32)>)> = self.cfgs.iter().map(|(&e, cfg)| (e, code_ranges(cfg))).collect();
        for (&entry, cfg) in &self.cfgs {
            if !x64 && cfg::is_delphi_move(self.mem, entry) {
                functions.function(types::FUNC);
                let name = abi::export_name(entry);
                exports.export(&name, ExportKind::Func, func_index[&entry]);
                code.function(&delphi_move());
                out_funcs.push((entry, name));
                continue;
            }
            let emit = |x87_inline: bool| {
                let mut fe = if x64 { lift::FuncEmit::new64(cfg, &func_index) } else { lift::FuncEmit::new(cfg, &func_index) };
                fe.probe = self.opts.probes.contains(&entry);
                fe.x87_inline = x87_inline && !x64;
                fe.resolve_cache = self.opts.resolve_cache;
                fe.count = self.opts.count;
                fe.count_blocks = self.opts.count_blocks;
                fe.emit();
                fe
            };
            let mut fe = emit(!self.opts.no_x87_inline);
            // Browsers reject function bodies over 7,654,321 bytes; x87 through rt.fpu
            // takes about half the space of the inline paths (d3dx9_43 has a 40,000
            // instruction function of x87 code: 7.8 MB, 3.6 MB this way).
            if fe.f.byte_len() > MAX_FUNCTION_BYTES && fe.x87_inline {
                fe = emit(false);
            }
            self.unimplemented.extend(fe.unimplemented.drain(..));
            functions.function(if x64 { types::FUNC64 } else { types::FUNC });
            let name = abi::export_name(entry);
            exports.export(&name, ExportKind::Func, func_index[&entry]);
            code.function(&fe.f);
            out_funcs.push((entry, name));
        }

        let mut names = wasm_encoder::NameMap::new();
        for (entry, name) in &out_funcs {
            names.append(func_index[entry], name);
        }

        let mut name_section = wasm_encoder::NameSection::new();
        name_section.functions(&names);

        let mut module = Module::new();
        module.section(&types);
        module.section(&imports);
        module.section(&functions);
        module.section(&exports);
        module.section(&code);
        module.section(&name_section);
        Output { wasm: module.finish(), functions: out_funcs, unimplemented: std::mem::take(&mut self.unimplemented), ranges }
    }
}

/// A flat byte image mapped at a base address: test programs and single modules.
pub struct FlatMem {
    pub base: u32,
    pub bytes: Vec<u8>,
}

impl GuestMem for FlatMem {
    fn read(&self, va: u32, buf: &mut [u8]) -> bool {
        let Some(off) = va.checked_sub(self.base) else { return false };
        match self.bytes.get(off as usize..off as usize + buf.len()) {
            Some(src) => {
                buf.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
}
