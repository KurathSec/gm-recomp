//! Runs a test case through the recompiler and wasmtime, mirroring refrun's layout.

use crate::proto::{CODE_BASE, CODE_MAX, CaseIn, CaseOut, DATA_BASE, DATA_SIZE, STACK_SIZE};
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

pub struct Host {
    pub trap: Option<(i32, u32)>,
    pub fpu_calls: u32,
    pub unresolved: Option<u32>,
    /// Guest entry address -> table index, for `resolve`.
    pub funcs: HashMap<u32, u32>,
}

pub struct Runner {
    engine: Engine,
}

pub struct RunResult {
    pub out: CaseOut,
    /// Instructions the translator could not handle.
    pub unimplemented: Vec<(u32, String)>,
    pub trap_kind: Option<i32>,
    pub wasm_error: Option<String>,
}

impl Runner {
    pub fn new() -> Result<Self> {
        let mut cfg = Config::new();
        cfg.wasm_tail_call(true);
        cfg.wasm_simd(true);
        cfg.wasm_bulk_memory(true);
        cfg.wasm_multi_value(true);
        Ok(Runner { engine: Engine::new(&cfg).map_err(wt)? })
    }

    pub fn translate(&self, case: &CaseIn, stack_base: u32, entries: &[u32]) -> Result<recomp::Output> {
        let mut code = case.code.clone();
        code.resize(CODE_MAX, 0xF4);
        let mem = SegMem { segs: vec![(DATA_BASE, case.data.clone()), (CODE_BASE, code), (stack_base, case.stack.clone())] };
        let mut tr = recomp::Translator::new(&mem, recomp::Options { shared_memory: false });
        let tables = HashMap::new();
        let entries_owned: Vec<u32> = entries.to_vec();
        let is_fn = move |a: u32| entries_owned.contains(&a);
        let info = FuncInfo { jump_tables: &tables, is_function: &is_fn };
        tr.add_function(CODE_BASE, &info).map_err(|e| anyhow!("{e}"))?;
        for &e in entries {
            tr.add_function(e, &info).map_err(|e| anyhow!("{e}"))?;
        }
        Ok(tr.finish())
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

        let mut store = Store::new(&self.engine, Host { trap: None, fpu_calls: 0, unresolved: None, funcs: HashMap::new() });
        let top = (stack_base as u64 + STACK_SIZE as u64).max(CODE_BASE as u64 + CODE_MAX as u64);
        let pages = top.div_ceil(65536) + 1;
        let memory = Memory::new(&mut store, MemoryType::new(pages as u32, None)).map_err(wt)?;
        let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 1, None), Ref::Func(None)).map_err(wt)?;
        let mut globals = Vec::new();
        for (_, _, ty) in GLOBALS {
            let (vt, init) = match ty {
                wasm_encoder::ValType::I32 => (ValType::I32, Val::I32(0)),
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
        let fpu = Func::wrap(&mut store, |mut caller: Caller<'_, Host>, _code: i32, _a: i32, _b: i32| -> i32 {
            caller.data_mut().fpu_calls += 1;
            0
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
        let func = instance.get_typed_func::<(), ()>(&mut store, &name).map_err(wt)?;
        let res = func.call(&mut store, ());
        let host = store.data();
        let trap_kind = host.trap.map(|t| t.0);
        let (status, fault_off) = match host.trap {
            Some((trap::HLT, ip)) => (0, ip - CODE_BASE),
            Some((trap::DIVIDE_ERROR, ip)) => (0xC000_0094, ip - CODE_BASE),
            Some((trap::INT3, ip)) => (0x8000_0003, ip - CODE_BASE),
            Some((trap::UD2, ip)) => (0xC000_001D, ip - CODE_BASE),
            Some((trap::INT, _)) => (0xC000_0005, u32::MAX),
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
