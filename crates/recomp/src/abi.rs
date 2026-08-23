//! The interface every recompiled module shares with the runtime and the harness.
//!
//! CPU state lives in mutable globals exported by a per-thread "cpu" instance and
//! imported by every code module, so functions in different modules see the same
//! registers. Guest memory is the imported memory 0; the function table maps
//! runtime-assigned indices to translated functions.

use wasm_encoder::ValType;

pub const CPU_MODULE: &str = "cpu";
pub const RT_MODULE: &str = "rt";
pub const MEMORY_NAME: &str = "mem";
pub const TABLE_NAME: &str = "fns";

/// Order defines the import index of each global.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Global {
    Eax = 0,
    Ecx,
    Edx,
    Ebx,
    Esp,
    Ebp,
    Esi,
    Edi,
    /// Address the guest wants to continue at after a `ret` or unresolved jump.
    Eip,
    Cf,
    Pf,
    Af,
    Zf,
    Sf,
    Of,
    Df,
    /// Linear base of the FS segment (the TEB).
    FsBase,
    Mxcsr,
    Xmm0,
    Xmm1,
    Xmm2,
    Xmm3,
    Xmm4,
    Xmm5,
    Xmm6,
    Xmm7,
}

pub const GLOBALS: [(Global, &str, ValType); 26] = [
    (Global::Eax, "eax", ValType::I32),
    (Global::Ecx, "ecx", ValType::I32),
    (Global::Edx, "edx", ValType::I32),
    (Global::Ebx, "ebx", ValType::I32),
    (Global::Esp, "esp", ValType::I32),
    (Global::Ebp, "ebp", ValType::I32),
    (Global::Esi, "esi", ValType::I32),
    (Global::Edi, "edi", ValType::I32),
    (Global::Eip, "eip", ValType::I32),
    (Global::Cf, "cf", ValType::I32),
    (Global::Pf, "pf", ValType::I32),
    (Global::Af, "af", ValType::I32),
    (Global::Zf, "zf", ValType::I32),
    (Global::Sf, "sf", ValType::I32),
    (Global::Of, "of", ValType::I32),
    (Global::Df, "df", ValType::I32),
    (Global::FsBase, "fs_base", ValType::I32),
    (Global::Mxcsr, "mxcsr", ValType::I32),
    (Global::Xmm0, "xmm0", ValType::V128),
    (Global::Xmm1, "xmm1", ValType::V128),
    (Global::Xmm2, "xmm2", ValType::V128),
    (Global::Xmm3, "xmm3", ValType::V128),
    (Global::Xmm4, "xmm4", ValType::V128),
    (Global::Xmm5, "xmm5", ValType::V128),
    (Global::Xmm6, "xmm6", ValType::V128),
    (Global::Xmm7, "xmm7", ValType::V128),
];

/// Runtime services imported by code modules, in import order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RtFunc {
    /// `resolve(addr) -> table index` of the function starting at `addr` (JIT-compiling if needed).
    Resolve = 0,
    /// `trap(kind, eip)`: the guest did something the translated code cannot continue from.
    Trap,
    /// `fpu(code, a, b) -> i32`: executes one x87/MMX instruction (`code` is the iced-x86 `Code`).
    Fpu,
    /// `cpuid()`: reads eax/ecx globals, writes eax..edx globals.
    Cpuid,
    /// `rdtsc()`: writes edx:eax globals from the virtual clock.
    Rdtsc,
}

pub const RT_FUNCS: [(RtFunc, &str); 5] = [
    (RtFunc::Resolve, "resolve"),
    (RtFunc::Trap, "trap"),
    (RtFunc::Fpu, "fpu"),
    (RtFunc::Cpuid, "cpuid"),
    (RtFunc::Rdtsc, "rdtsc"),
];

/// `trap` kinds.
pub mod trap {
    pub const UNIMPLEMENTED: i32 = 1;
    pub const INT3: i32 = 2;
    /// `int n` with n in the low byte of `eip`'s companion argument.
    pub const INT: i32 = 3;
    pub const DIVIDE_ERROR: i32 = 4;
    pub const UD2: i32 = 5;
    pub const HLT: i32 = 6;
    /// A branch left the translated region without a resolvable target.
    pub const NO_TARGET: i32 = 7;
}

/// Type indices inside every generated module.
pub mod types {
    /// `() -> ()`: translated guest functions, cpuid, rdtsc.
    pub const FUNC: u32 = 0;
    /// `(i32) -> i32`: resolve.
    pub const RESOLVE: u32 = 1;
    /// `(i32, i32) -> ()`: trap.
    pub const TRAP: u32 = 2;
    /// `(i32, i32, i32) -> i32`: fpu.
    pub const FPU: u32 = 3;
}

pub fn export_name(entry_va: u32) -> String {
    format!("f_{entry_va:08x}")
}
