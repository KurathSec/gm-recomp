//! x86 (32-bit) to WebAssembly static recompiler.

pub mod abi;
pub mod cfg;
pub mod flags;
pub mod lift;
pub mod sse;

use abi::{GLOBALS, RT_FUNCS, types};
use cfg::{Cfg, FuncInfo, GuestMem};
use std::collections::{BTreeMap, HashMap};
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
}

pub struct Translator<'m> {
    mem: &'m dyn GuestMem,
    opts: Options,
    cfgs: BTreeMap<u32, Cfg>,
    pub unimplemented: Vec<(u32, String)>,
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
        Translator { mem, opts, cfgs: BTreeMap::new(), unimplemented: Vec::new() }
    }

    pub fn add_function(&mut self, entry: u32, info: &FuncInfo) -> Result<(), String> {
        if self.cfgs.contains_key(&entry) {
            return Ok(());
        }
        let cfg = cfg::build(self.mem, entry, info)?;
        self.cfgs.insert(entry, cfg);
        Ok(())
    }

    pub fn finish(mut self) -> Output {
        let n_rt = RT_FUNCS.len() as u32;
        let func_index: HashMap<u32, u32> = self.cfgs.keys().enumerate().map(|(i, &e)| (e, n_rt + i as u32)).collect();

        let mut types = TypeSection::new();
        types.ty().function([], []); // FUNC
        types.ty().function([ValType::I32], [ValType::I32]); // RESOLVE
        types.ty().function([ValType::I32, ValType::I32], []); // TRAP
        types.ty().function([ValType::I32; 5], [ValType::I32]); // FPU
        debug_assert_eq!(types::FPU, 3);

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
                _ => types::FUNC,
            };
            imports.import(abi::RT_MODULE, name, EntityType::Function(ty));
        }

        let mut functions = FunctionSection::new();
        let mut exports = ExportSection::new();
        let mut code = CodeSection::new();
        let mut out_funcs = Vec::new();
        for (&entry, cfg) in &self.cfgs {
            let mut fe = lift::FuncEmit::new(cfg, &func_index);
            fe.probe = self.opts.probes.contains(&entry);
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
