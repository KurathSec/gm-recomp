//! Runs a test case through the recompiler and wasmtime, mirroring refrun's layout.

use crate::proto::{CODE_BASE, CODE_MAX, CaseIn, CaseIn64, CaseOut, CaseOut64, DATA_BASE, DATA_SIZE, STACK_SIZE};
use anyhow::{Context, Result, anyhow};
use wasmtime::Error as WtError;
use recomp::abi::{self, GLOBALS, RT_FUNCS, trap};
use recomp::cfg::FuncInfo;
use std::collections::HashMap;
use wasmtime::{
    Caller, Config, Engine, Extern, Func, Global, GlobalType, Linker, Memory, MemoryType, Module, Mutability, Ref, RefType,
    Store, Table, TableType, Val, ValType,
};

/// Guest image made of a few disjoint segments.
pub struct SegMem {
    pub segs: Vec<(u32, Vec<u8>)>,
}

impl recomp::Memory for SegMem {
    fn read(&self, va: u32, buf: &mut [u8]) -> bool {
        for (base, bytes) in &self.segs {
            if let Some(off) = va.checked_sub(*base) {
                if let Some(src) = bytes.get(off as usize..off as usize + buf.len()) {
                    buf.copy_from_slice(src);
                    return true;
                }
            }
        }
        false
    }
}

/// Where the test's x87 state block lives (fs_base = FPU_BLOCK + fpu::block::OFFSET),
/// clear of the data, code and stack windows.
pub const FPU_BLOCK: u32 = 0x2000_3000;

pub struct Host {
    pub trap: Option<(i32, u32)>,
    pub fpu_calls: u32,
    pub unresolved: Option<u32>,
    /// Guest entry address -> table index, for `resolve`.
    pub funcs: HashMap<u32, u32>,
}

fn block(mem: &[u8]) -> &[u8] {
    &mem[FPU_BLOCK as usize..FPU_BLOCK as usize + fpu::block::SIZE]
}

struct MemView<'a>(&'a mut [u8]);
impl fpu::Mem for MemView<'_> {
    fn read(&mut self, addr: u32, buf: &mut [u8]) {
        let a = addr as usize;
        if let Some(src) = self.0.get(a..a + buf.len()) {
            buf.copy_from_slice(src);
        }
    }
    fn write(&mut self, addr: u32, data: &[u8]) {
        let a = addr as usize;
        if let Some(dst) = self.0.get_mut(a..a + data.len()) {
            dst.copy_from_slice(data);
        }
    }
}

pub struct Runner {
    engine: Engine,
    /// The x87 fast-path module, compiled once.
    x87: Module,
}

pub struct RunResult {
    pub out: CaseOut,
    /// Instructions the translator could not handle.
    pub unimplemented: Vec<(u32, String)>,
    pub trap_kind: Option<i32>,
    pub wasm_error: Option<String>,
}

pub struct RunResult64 {
    pub out: Box<CaseOut64>,
    pub unimplemented: Vec<(u32, String)>,
    pub wasm_error: Option<String>,
}

impl Runner {
    pub fn new() -> Result<Self> {
        let mut cfg = Config::new();
        cfg.wasm_tail_call(true);
        cfg.wasm_simd(true);
        cfg.wasm_bulk_memory(true);
        cfg.wasm_multi_value(true);
        let engine = Engine::new(&cfg).map_err(wt)?;
        let x87 = Module::new(&engine, recomp::x87::module(false)).map_err(wt)?;
        Ok(Runner { engine, x87 })
    }

    pub fn translate(&self, case: &CaseIn, stack_base: u32, entries: &[u32]) -> Result<recomp::Output> {
        let mut code = case.code.clone();
        code.resize(CODE_MAX, 0xF4);
        let mem = SegMem { segs: vec![(DATA_BASE, case.data.clone()), (CODE_BASE, code), (stack_base, case.stack.clone())] };
        let no_x87_inline = std::env::var_os("GMR_X87_SLOW").is_some();
        let mut tr = recomp::Translator::new(&mem, recomp::Options { shared_memory: false, no_x87_inline, ..Default::default() });
        let tables = HashMap::new();
        let entries_owned: Vec<u32> = entries.to_vec();
        let is_fn = move |a: u32| entries_owned.contains(&a);
        let info = FuncInfo { jump_tables: &tables, is_function: &is_fn, translated: &|_| None, high: 0, volatile: &|_| false, tag: 0, max_instrs: 0 };
        tr.add_function(CODE_BASE, &info).map_err(|e| anyhow!("{e}"))?;
        for &e in entries {
            tr.add_function(e, &info).map_err(|e| anyhow!("{e}"))?;
        }
        Ok(tr.finish())
    }

    /// Translates a 64-bit test program (its functions typed FUNC64, decoded with the
    /// image's upper address half zero as in the tests).
    pub fn translate64(&self, case: &CaseIn64, stack_base: u32, entries: &[u32]) -> Result<recomp::Output> {
        let mem = SegMem {
            segs: vec![(DATA_BASE, case.data.to_vec()), (CODE_BASE, case.code.to_vec()), (stack_base, case.stack.to_vec())],
        };
        let mut tr = recomp::Translator::new(&mem, recomp::Options { x64: true, ..Default::default() });
        let tables = HashMap::new();
        let entries_owned: Vec<u32> = entries.to_vec();
        let is_fn = move |a: u32| entries_owned.contains(&a);
        let info = FuncInfo { jump_tables: &tables, is_function: &is_fn, translated: &|_| None, high: 0, volatile: &|_| false, tag: 0, max_instrs: 0 };
        tr.add_function(CODE_BASE, &info).map_err(|e| anyhow!("{e}"))?;
        for &e in entries {
            tr.add_function(e, &info).map_err(|e| anyhow!("{e}"))?;
        }
        Ok(tr.finish())
    }

    /// Runs a 64-bit test case: translated FUNC64 functions, the sixteen 64-bit registers
    /// in the globals (rsp into the stack window). Mirrors `run`.
    pub fn run64(&self, case: &CaseIn64, stack_base: u32, entries: &[u32]) -> Result<RunResult64> {
        use recomp::abi::Global as G;
        let output = self.translate64(case, stack_base, entries)?;
        let unimplemented = output.unimplemented.clone();
        let module = match Module::new(&self.engine, &output.wasm) {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::write("target/refrun/failed64.wasm", &output.wasm);
                return Err(wt(e)).context("compiling generated 64-bit wasm (module written to target/refrun/failed64.wasm)");
            }
        };
        let host = Host { trap: None, fpu_calls: 0, unresolved: None, funcs: HashMap::new() };
        let mut store = Store::new(&self.engine, host);
        let top = (stack_base as u64 + STACK_SIZE as u64).max(CODE_BASE as u64 + CODE_MAX as u64);
        let pages = top.div_ceil(65536) + 1;
        let memory = Memory::new(&mut store, MemoryType::new(pages as u32, None)).map_err(wt)?;
        let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 1, None), Ref::Func(None)).map_err(wt)?;
        let mut globals = Vec::new();
        for (_, _, ty) in GLOBALS {
            let (vt, init) = match ty {
                wasm_encoder::ValType::I32 => (ValType::I32, Val::I32(0)),
                wasm_encoder::ValType::I64 => (ValType::I64, Val::I64(0)),
                wasm_encoder::ValType::V128 => (ValType::V128, Val::V128(0u128.into())),
                _ => unreachable!(),
            };
            globals.push(Global::new(&mut store, GlobalType::new(vt, Mutability::Var), init).map_err(wt)?);
        }
        let mut linker = Linker::new(&self.engine);
        linker.define(&store, abi::CPU_MODULE, abi::MEMORY_NAME, Extern::Memory(memory)).map_err(wt)?;
        linker.define(&store, abi::CPU_MODULE, abi::TABLE_NAME, Extern::Table(table)).map_err(wt)?;
        for ((_, name, _), g) in GLOBALS.iter().zip(&globals) {
            linker.define(&store, abi::CPU_MODULE, name, Extern::Global(*g)).map_err(wt)?;
        }
        let resolve = Func::wrap(&mut store, |mut caller: Caller<'_, Host>, addr: i32| -> wasmtime::Result<i32> {
            if let Some(&idx) = caller.data().funcs.get(&(addr as u32)) {
                return Ok(idx as i32);
            }
            caller.data_mut().unresolved = Some(addr as u32);
            Err(WtError::msg(format!("resolve({addr:#x}): no such function in the test module")))
        });
        let trap_fn = Func::wrap(&mut store, |mut caller: Caller<'_, Host>, kind: i32, arg: i32| -> wasmtime::Result<()> {
            caller.data_mut().trap = Some((kind, arg as u32));
            Err(WtError::msg(format!("guest trap {kind}")))
        });
        // (64-bit code never calls rt.fpu; a stub satisfies the import)
        let fpu = Func::wrap(&mut store, |_: Caller<'_, Host>, _: i32, _: i32, _: i32, _: i32, _: i32| -> wasmtime::Result<i32> { Ok(0) });
        let cpuid = Func::wrap(&mut store, |_caller: Caller<'_, Host>| {});
        let rdtsc = Func::wrap(&mut store, |_caller: Caller<'_, Host>| {});
        for (f, name) in RT_FUNCS {
            let func = match f {
                abi::RtFunc::Resolve => resolve,
                abi::RtFunc::Trap => trap_fn,
                abi::RtFunc::Fpu => fpu,
                abi::RtFunc::Cpuid => cpuid,
                abi::RtFunc::Rdtsc => rdtsc,
            };
            linker.define(&store, abi::RT_MODULE, name, Extern::Func(func)).map_err(wt)?;
        }
        let instance = linker.instantiate(&mut store, &module).map_err(wt).context("instantiating generated 64-bit wasm")?;
        for (entry, name) in &output.functions {
            let f = instance.get_func(&mut store, name).context("missing export")?;
            let idx = table.grow(&mut store, 1, Ref::Func(Some(f))).map_err(wt)?;
            store.data_mut().funcs.insert(*entry, idx as u32);
        }

        memory.write(&mut store, DATA_BASE as usize, &case.data).map_err(|e| anyhow!("{e:?}"))?;
        memory.write(&mut store, CODE_BASE as usize, &case.code).map_err(|e| anyhow!("{e:?}"))?;
        memory.write(&mut store, stack_base as usize, &case.stack).map_err(|e| anyhow!("{e:?}"))?;
        let mut f = fpu::Fpu::new();
        f.read_fxsave(&case.fx);
        let mut img = [0u8; fpu::block::SIZE];
        f.to_block(&mut img);
        memory.write(&mut store, FPU_BLOCK as usize, &img).map_err(|e| anyhow!("{e:?}"))?;
        globals[G::FsBase as usize].set(&mut store, Val::I32((FPU_BLOCK + fpu::block::OFFSET) as i32)).map_err(wt)?;
        globals[G::GsBase as usize].set(&mut store, Val::I32(0)).map_err(wt)?;
        for r in 0..16 {
            let v = if r == 4 { stack_base as u64 + (case.regs[4] & (STACK_SIZE as u64 - 1)) } else { case.regs[r] };
            globals[G::Rax as usize + r].set(&mut store, Val::I64(v as i64)).map_err(wt)?;
        }
        let fl = case.rflags;
        for (g, bit) in [(G::Cf, 0), (G::Pf, 2), (G::Af, 4), (G::Zf, 6), (G::Sf, 7), (G::Of, 11), (G::Df, 10)] {
            globals[g as usize].set(&mut store, Val::I32(((fl >> bit) & 1) as i32)).map_err(wt)?;
        }
        let mxcsr = u32::from_le_bytes(case.fx[24..28].try_into().unwrap());
        globals[G::Mxcsr as usize].set(&mut store, Val::I32(mxcsr as i32)).map_err(wt)?;
        for x in 0..16 {
            let bytes: [u8; 16] = case.fx[160 + x * 16..176 + x * 16].try_into().unwrap();
            globals[abi::xmm_global(x as u32) as usize].set(&mut store, Val::V128(u128::from_le_bytes(bytes).into())).map_err(wt)?;
        }

        let name = abi::export_name(CODE_BASE);
        let func = instance.get_func(&mut store, &name).ok_or_else(|| anyhow!("missing entry export"))?;
        let args: Vec<Val> = (0..16).map(|r| globals[G::Rax as usize + r].get(&mut store)).collect();
        let mut res = vec![Val::I64(0); 16];
        let r = func.call(&mut store, &args, &mut res).map(|()| {
            for (r, v) in res.iter().enumerate() {
                let _ = globals[G::Rax as usize + r].set(&mut store, v.clone());
            }
        });
        let host = store.data();
        let (status, fault_off) = match host.trap {
            Some((trap::HLT, ip)) => (0, ip - CODE_BASE),
            Some((trap::DIVIDE_ERROR, ip)) => (0xC000_0094, ip - CODE_BASE),
            Some((trap::INT3, ip)) => (0x8000_0003, ip - CODE_BASE),
            Some((trap::UD2, ip)) => (0xC000_001D, ip - CODE_BASE),
            Some((trap::INT, _)) => (0xC000_0005, u32::MAX),
            Some((trap::UNIMPLEMENTED, ip)) => (0xFFFF_0001, ip - CODE_BASE),
            Some((_, ip)) => (0xFFFF_0002, ip.wrapping_sub(CODE_BASE)),
            None => match &r {
                Ok(()) => (0xFFFF_0003, u32::MAX),
                Err(_) => (0xC000_0005, u32::MAX),
            },
        };
        let wasm_error = match (&r, host.trap) {
            (Err(e), None) => Some(format!("{e:#}")),
            _ => None,
        };
        let mut out = CaseOut64::zeroed();
        out.status = status;
        out.fault_off = fault_off;
        for r in 0..16 {
            out.regs[r] = globals[G::Rax as usize + r].get(&mut store).i64().unwrap() as u64;
        }
        let bit = |store: &mut Store<Host>, g: G, shift: u32| -> u64 { (globals[g as usize].get(store).i32().unwrap() as u64 & 1) << shift };
        out.rflags = 0x202
            | bit(&mut store, G::Cf, 0)
            | bit(&mut store, G::Pf, 2)
            | bit(&mut store, G::Af, 4)
            | bit(&mut store, G::Zf, 6)
            | bit(&mut store, G::Sf, 7)
            | bit(&mut store, G::Df, 10)
            | bit(&mut store, G::Of, 11);
        out.fx = case.fx;
        out.fx[24..28].copy_from_slice(&(globals[G::Mxcsr as usize].get(&mut store).i32().unwrap() as u32).to_le_bytes());
        for x in 0..16 {
            let v = globals[abi::xmm_global(x as u32) as usize].get(&mut store).v128().unwrap().as_u128();
            out.fx[160 + x * 16..176 + x * 16].copy_from_slice(&v.to_le_bytes());
        }
        memory.read(&store, DATA_BASE as usize, &mut out.data).map_err(|e| anyhow!("{e:?}"))?;
        memory.read(&store, stack_base as usize, &mut out.stack).map_err(|e| anyhow!("{e:?}"))?;
        Ok(RunResult64 { out, unimplemented, wasm_error })
    }

    pub fn run(&self, case: &CaseIn, stack_base: u32, entries: &[u32]) -> Result<RunResult> {
        let output = self.translate(case, stack_base, entries)?;
        let unimplemented = output.unimplemented.clone();
        let module = match Module::new(&self.engine, &output.wasm) {
            Ok(m) => m,
            Err(e) => {
                let _ = std::fs::write("target/refrun/failed.wasm", &output.wasm);
                return Err(wt(e)).context("compiling generated wasm (module written to target/refrun/failed.wasm)");
            }
        };

        let host = Host { trap: None, fpu_calls: 0, unresolved: None, funcs: HashMap::new() };
        let mut store = Store::new(&self.engine, host);
        let top = (stack_base as u64 + STACK_SIZE as u64).max(CODE_BASE as u64 + CODE_MAX as u64);
        let pages = top.div_ceil(65536) + 1;
        let memory = Memory::new(&mut store, MemoryType::new(pages as u32, None)).map_err(wt)?;
        let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 1, None), Ref::Func(None)).map_err(wt)?;
        let mut globals = Vec::new();
        for (_, _, ty) in GLOBALS {
            let (vt, init) = match ty {
                wasm_encoder::ValType::I32 => (ValType::I32, Val::I32(0)),
                wasm_encoder::ValType::I64 => (ValType::I64, Val::I64(0)),
                wasm_encoder::ValType::V128 => (ValType::V128, Val::V128(0u128.into())),
                _ => unreachable!(),
            };
            globals.push(Global::new(&mut store, GlobalType::new(vt, Mutability::Var), init).map_err(wt)?);
        }

        let mut linker = Linker::new(&self.engine);
        linker.define(&store, abi::CPU_MODULE, abi::MEMORY_NAME, Extern::Memory(memory)).map_err(wt)?;
        linker.define(&store, abi::CPU_MODULE, abi::TABLE_NAME, Extern::Table(table)).map_err(wt)?;
        for ((_, name, _), g) in GLOBALS.iter().zip(&globals) {
            linker.define(&store, abi::CPU_MODULE, name, Extern::Global(*g)).map_err(wt)?;
        }
        let resolve = Func::wrap(&mut store, |mut caller: Caller<'_, Host>, addr: i32| -> wasmtime::Result<i32> {
            if let Some(&idx) = caller.data().funcs.get(&(addr as u32)) {
                return Ok(idx as i32);
            }
            caller.data_mut().unresolved = Some(addr as u32);
            Err(WtError::msg(format!("resolve({addr:#x}): no such function in the test module")))
        });
        let trap_fn = Func::wrap(&mut store, |mut caller: Caller<'_, Host>, kind: i32, arg: i32| -> wasmtime::Result<()> {
            caller.data_mut().trap = Some((kind, arg as u32));
            // Every trap kind ends a test case, like the native exception does.
            Err(WtError::msg(format!("guest trap {kind}")))
        });
        let fpu_mem = memory;
        let fpu = Func::wrap(&mut store, move |mut caller: Caller<'_, Host>, code: i32, a: i32, b: i32, ip: i32, fop: i32| -> wasmtime::Result<i32> {
            caller.data_mut().fpu_calls += 1;
            let mn = iced_x86::Code::try_from(code as usize).map(|c| c.mnemonic()).unwrap_or(iced_x86::Mnemonic::INVALID);
            let mut f = fpu::Fpu::from_block(block(fpu_mem.data(&caller)));
            if f.fault_pending(mn) {
                caller.data_mut().trap = Some((trap::FPU_EXCEPTION, ip as u32));
                return Err(WtError::msg("x87 exception pending"));
            }
            let mem = fpu_mem.data_mut(&mut caller);
            let r = f.exec(mn, a as u32, b as u32, ip as u32, fop as u16, &mut MemView(mem));
            f.to_block(&mut mem[FPU_BLOCK as usize..FPU_BLOCK as usize + fpu::block::SIZE]);
            Ok(r)
        });
        let cpuid = Func::wrap(&mut store, |_caller: Caller<'_, Host>| {});
        let rdtsc = Func::wrap(&mut store, |_caller: Caller<'_, Host>| {});
        for (f, name) in RT_FUNCS {
            let func = match f {
                abi::RtFunc::Resolve => resolve,
                abi::RtFunc::Trap => trap_fn,
                abi::RtFunc::Fpu => fpu,
                abi::RtFunc::Cpuid => cpuid,
                abi::RtFunc::Rdtsc => rdtsc,
            };
            linker.define(&store, abi::RT_MODULE, name, Extern::Func(func)).map_err(wt)?;
        }
        linker.func_wrap(abi::RT_MODULE, "sin", fpu::sin).map_err(wt)?;
        linker.func_wrap(abi::RT_MODULE, "cos", fpu::cos).map_err(wt)?;
        let x87 = linker.instantiate(&mut store, &self.x87).map_err(wt)?;
        linker.instance(&mut store, recomp::x87::MODULE, x87).map_err(wt)?;
        let instance = linker.instantiate(&mut store, &module).map_err(wt).context("instantiating generated wasm")?;
        // Every translated function gets a table slot so `call reg` can reach it.
        for (entry, name) in &output.functions {
            let f = instance.get_func(&mut store, name).context("missing export")?;
            let idx = table.grow(&mut store, 1, Ref::Func(Some(f))).map_err(wt)?;
            store.data_mut().funcs.insert(*entry, idx as u32);
        }

        // Initial state.
        memory.write(&mut store, DATA_BASE as usize, &case.data).map_err(|e| anyhow!("{e:?}"))?;
        let mut code = case.code.clone();
        code.resize(CODE_MAX, 0xF4);
        memory.write(&mut store, CODE_BASE as usize, &code).map_err(|e| anyhow!("{e:?}"))?;
        memory.write(&mut store, stack_base as usize, &case.stack).map_err(|e| anyhow!("{e:?}"))?;
        let mut f = fpu::Fpu::new();
        f.read_fxsave(&case.fx);
        let mut img = [0u8; fpu::block::SIZE];
        f.to_block(&mut img);
        memory.write(&mut store, FPU_BLOCK as usize, &img).map_err(|e| anyhow!("{e:?}"))?;
        globals[abi::Global::FsBase as usize].set(&mut store, Val::I32((FPU_BLOCK + fpu::block::OFFSET) as i32)).map_err(wt)?;
        for r in 0..8 {
            let v = if r == 4 { stack_base + (case.regs[4] & (STACK_SIZE as u32 - 1)) } else { case.regs[r] };
            globals[r].set(&mut store, Val::I32(v as i32)).map_err(wt)?;
        }
        let fl = case.eflags;
        for (g, bit) in [(abi::Global::Cf, 0), (abi::Global::Pf, 2), (abi::Global::Af, 4), (abi::Global::Zf, 6), (abi::Global::Sf, 7), (abi::Global::Of, 11), (abi::Global::Df, 10)] {
            globals[g as usize].set(&mut store, Val::I32(((fl >> bit) & 1) as i32)).map_err(wt)?;
        }
        let mxcsr = u32::from_le_bytes(case.fx[24..28].try_into().unwrap());
        globals[abi::Global::Mxcsr as usize].set(&mut store, Val::I32(mxcsr as i32)).map_err(wt)?;
        for x in 0..8 {
            let bytes: [u8; 16] = case.fx[160 + x * 16..176 + x * 16].try_into().unwrap();
            globals[abi::Global::Xmm0 as usize + x].set(&mut store, Val::V128(u128::from_le_bytes(bytes).into())).map_err(wt)?;
        }

        let name = abi::export_name(CODE_BASE);
        type Regs = (i32, i32, i32, i32, i32, i32, i32, i32);
        let func = instance.get_typed_func::<Regs, Regs>(&mut store, &name).map_err(wt)?;
        let g = |store: &mut Store<Host>, r: usize| globals[r].get(store).i32().unwrap();
        let args = (g(&mut store, 0), g(&mut store, 1), g(&mut store, 2), g(&mut store, 3), g(&mut store, 4), g(&mut store, 5), g(&mut store, 6), g(&mut store, 7));
        let res = func.call(&mut store, args).map(|r| {
            for (k, v) in [r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7].into_iter().enumerate() {
                let _ = globals[k].set(&mut store, Val::I32(v));
            }
        });
        let host = store.data();
        let trap_kind = host.trap.map(|t| t.0);
        let (status, fault_off) = match host.trap {
            Some((trap::HLT, ip)) => (0, ip - CODE_BASE),
            Some((trap::DIVIDE_ERROR, ip)) => (0xC000_0094, ip - CODE_BASE),
            Some((trap::INT3, ip)) => (0x8000_0003, ip - CODE_BASE),
            Some((trap::UD2, ip)) => (0xC000_001D, ip - CODE_BASE),
            Some((trap::INT, _)) => (0xC000_0005, u32::MAX),
            Some((trap::FPU_EXCEPTION, ip)) => (0xC000_0094, ip - CODE_BASE),
            Some((trap::UNIMPLEMENTED, ip)) => (0xFFFF_0001, ip - CODE_BASE),
            Some((_, ip)) => (0xFFFF_0002, ip.wrapping_sub(CODE_BASE)),
            None => match &res {
                Ok(()) => (0xFFFF_0003, u32::MAX), // returned without hlt: should not happen
                Err(_) => (0xC000_0005, u32::MAX),  // wasm trap (out-of-bounds etc.)
            },
        };
        let wasm_error = match (&res, host.trap) {
            (Err(e), None) => Some(format!("{e:#}")),
            _ => None,
        };

        let mut regs = [0u32; 8];
        for r in 0..8 {
            regs[r] = globals[r].get(&mut store).i32().unwrap() as u32;
        }
        let bit = |store: &mut Store<Host>, g: abi::Global, shift: u32| -> u32 {
            (globals[g as usize].get(store).i32().unwrap() as u32 & 1) << shift
        };
        let eflags = 0x202
            | bit(&mut store, abi::Global::Cf, 0)
            | bit(&mut store, abi::Global::Pf, 2)
            | bit(&mut store, abi::Global::Af, 4)
            | bit(&mut store, abi::Global::Zf, 6)
            | bit(&mut store, abi::Global::Sf, 7)
            | bit(&mut store, abi::Global::Df, 10)
            | bit(&mut store, abi::Global::Of, 11);
        let mut fx = case.fx;
        fpu::Fpu::from_block(block(memory.data(&store))).write_fxsave(&mut fx);
        fx[24..28].copy_from_slice(&(globals[abi::Global::Mxcsr as usize].get(&mut store).i32().unwrap() as u32).to_le_bytes());
        for x in 0..8 {
            let v = globals[abi::Global::Xmm0 as usize + x].get(&mut store).v128().unwrap().as_u128();
            fx[160 + x * 16..176 + x * 16].copy_from_slice(&v.to_le_bytes());
        }
        let mut data = vec![0u8; DATA_SIZE];
        memory.read(&store, DATA_BASE as usize, &mut data).map_err(|e| anyhow!("{e:?}"))?;
        let mut stack = vec![0u8; STACK_SIZE];
        memory.read(&store, stack_base as usize, &mut stack).map_err(|e| anyhow!("{e:?}"))?;
        Ok(RunResult {
            out: CaseOut { status, fault_off, stack_base, regs, eflags, fx, data, stack },
            unimplemented,
            trap_kind,
            wasm_error,
        })
    }
}

fn wt(e: WtError) -> anyhow::Error {
    anyhow!("{e:?}")
}
