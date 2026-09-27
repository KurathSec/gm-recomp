//! x86 (32-bit) to WebAssembly static recompiler.

pub mod abi;
pub mod cfg;
pub mod fasthash;
pub mod flags;
pub mod layout;
pub mod lift;
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
}

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
        let cfg = cfg::build(self.mem, entry, info, !self.opts.no_inline, &self.cache)?;
        self.cfgs.insert(entry, cfg);
        Ok(())
    }

    /// Entries of the functions added so far.
    pub fn entries(&self) -> impl Iterator<Item = u32> + '_ {
        self.cfgs.keys().copied()
    }

    /// Functions the one at `entry` (added) calls or jumps to directly.
    pub fn direct_targets(&self, entry: u32) -> Vec<u32> {
        let Some(cfg) = self.cfgs.get(&entry) else { return Vec::new() };
        let mut out = Vec::new();
        for b in cfg.blocks.values() {
            for i in &b.instrs {
                if i.flow_control() == iced_x86::FlowControl::Call && i.op0_kind() == iced_x86::OpKind::NearBranch32 {
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
        // Imported functions: the runtime services, then every x87 fast path.
        let n_rt = (RT_FUNCS.len() + x87::FORMS.len()) as u32;
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
        for (_, name, ty) in GLOBALS {
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
        for k in 0..x87::FORMS.len() {
            imports.import(x87::MODULE, &x87::export_name(k), EntityType::Function(types::X87));
        }

        let mut functions = FunctionSection::new();
        let mut exports = ExportSection::new();
        let mut code = CodeSection::new();
        let mut out_funcs = Vec::new();
        for (&entry, cfg) in &self.cfgs {
            let mut fe = lift::FuncEmit::new(cfg, &func_index);
            fe.probe = self.opts.probes.contains(&entry);
            fe.x87_inline = !self.opts.no_x87_inline;
            fe.resolve_cache = self.opts.resolve_cache;
            fe.count = self.opts.count;
            fe.count_blocks = self.opts.count_blocks;
            fe.emit();
            self.unimplemented.extend(fe.unimplemented.drain(..));
            functions.function(types::FUNC);
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
        Output { wasm: module.finish(), functions: out_funcs, unimplemented: std::mem::take(&mut self.unimplemented) }
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
