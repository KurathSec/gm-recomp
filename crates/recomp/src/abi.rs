//! The interface every recompiled module shares with the runtime and the harness.
//!
//! CPU state lives in mutable globals exported by a per-thread "cpu" instance and
//! imported by every code module, so functions in different modules see the same
//! registers. Guest memory is the imported memory 0; the function table maps
//! runtime-assigned indices to translated functions.

use wasm_encoder::ValType;

/// Direct-mapped cache of `rt.resolve` results in guest memory (reserved by the
/// runtime): entry k is [guest address, table index] at `RESOLVE_CACHE + 8 * k`.
/// Translated code probes it before calling `rt.resolve`; the runtime fills it with
/// translated functions only (never API stubs, which must reach the runtime).
pub const RESOLVE_CACHE: u32 = 0x7F20_0000;
pub const RESOLVE_CACHE_BITS: u32 = 16;
pub const RESOLVE_CACHE_SIZE: u32 = 8 << RESOLVE_CACHE_BITS;
/// Event counters translated code bumps when `Options::count` is on (debugging): u32
/// each, indexed by `COUNT_*`, right after the resolve cache.
pub const COUNTERS: u32 = RESOLVE_CACHE + RESOLVE_CACHE_SIZE;
pub const COUNT_ENTRIES: u32 = 0;
pub const COUNT_CALLS: u32 = 1;
pub const COUNT_X87: u32 = 2;
pub const COUNT_RESOLVES: u32 = 3;
pub const COUNT_NAMES: [&str; 4] = ["function entries", "calls", "x87 instructions", "resolve probes"];
pub const RESOLVE_HASH: u32 = 0x9E37_79B1;

/// Byte offset of the cache entry for `addr` (what translated code computes).
pub fn resolve_slot(addr: u32) -> u32 {
    (addr.wrapping_mul(RESOLVE_HASH) >> (32 - RESOLVE_CACHE_BITS)) << 3
}

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
    /// Nonzero: stop running translated code and return to the dispatcher (the runtime
    /// recorded why: a fault to raise, a context to resume, or a stop).
    Ctl,
}

pub const GLOBALS: [(Global, &str, ValType); 27] = [
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
    (Global::Ctl, "ctl", ValType::I32),
];

/// Runtime services imported by code modules, in import order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RtFunc {
    /// `resolve(addr) -> table index` of the function starting at `addr` (JIT-compiling if needed).
    Resolve = 0,
    /// `trap(kind, eip)`: the guest did something the translated code cannot continue from.
    Trap,
    /// `fpu(code, a, b, ip, fop) -> i32`: executes one x87/MMX instruction (`code` is the
    /// iced-x86 `Code`; `ip` and the 11-bit `fop` feed the FPU instruction/opcode pointers).
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
    /// A waiting x87 instruction found an unmasked exception pending (#MF).
    pub const FPU_EXCEPTION: i32 = 8;
    /// Debugging: execution reached the entry of a probed function (`arg` = address).
    pub const PROBE: i32 = 9;
}

/// Type indices inside every generated module.
pub mod types {
    /// `(eax, ecx, edx, ebx, esp, ebp, esi, edi) -> (the same)`: translated guest
    /// functions take the registers and return them.
    pub const FUNC: u32 = 0;
    /// `(i32) -> i32`: resolve.
    pub const RESOLVE: u32 = 1;
    /// `(i32, i32) -> ()`: trap.
    pub const TRAP: u32 = 2;
    /// `(i32, i32, i32, i32, i32) -> i32`: fpu.
    pub const FPU: u32 = 3;
    /// `(i32, i32) -> i32`: the x87 module's fast paths.
    pub const X87: u32 = 4;
    /// `() -> ()`: cpuid, rdtsc.
    pub const VOID: u32 = 5;
}

pub fn export_name(entry_va: u32) -> String {
    format!("f_{entry_va:08x}")
}
