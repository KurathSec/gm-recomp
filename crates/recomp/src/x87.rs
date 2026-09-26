//! Fast x87. The thread's FPU state lives in guest memory (`fpu::block`), with every
//! register that holds a zero, an infinity or a normal double stored as f64. The
//! common instructions run in the shared `x87` module on those registers when the
//! f64 result is provably the one the 80-bit unit produces: precision control 53
//! bits for arithmetic, no denormals, NaNs, overflow or underflow, no stack faults.
//! The precision flag and C1 ("rounded up") come from exact error terms (TwoSum,
//! Dekker's product); the directed rounding modes move the round-to-nearest result
//! one step toward the exact value when the error's sign says it lies on the wrong
//! side. Anything a check rejects runs
//! through `rt.fpu` on the same state, so both paths agree by construction.
//!
//! Translated code calls one helper per instruction form (`f<k>(a, ip) -> i32`, `a`
//! the memory address or ST index) and falls back to `rt.fpu` when it returns -1;
//! otherwise the result is what `rt.fpu` would have returned (the status word for
//! fnstsw, EFLAGS bits for fcomi). Keeping the bodies out of line keeps translated
//! functions small, which matters for compile time.

use crate::abi;
use fpu::block as fb;
use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind};
use std::sync::LazyLock;
use wasm_encoder::{
    BlockType, CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, GlobalType, ImportSection,
    Instruction as W, MemArg, MemoryType, Module, TypeSection, ValType,
};

/// Import module name of the helpers.
pub const MODULE: &str = "x87";

const SW_PE_SHIFT: u32 = 5;
const SW_C1: u32 = 1 << 9;
const SW_CC: u32 = 0x4700;

const MIN_NORMAL: f64 = f64::MIN_POSITIVE;
/// 2^e as f64 (normal range).
const fn pow2(e: i64) -> f64 {
    f64::from_bits(((1023 + e) as u64) << 52)
}
const F32_MIN_NORMAL: f64 = pow2(-126);
/// Below this, rounding to single might reach the subnormal range.
const F32_TINY: f64 = pow2(-125);
const F32_MAX: f64 = f32::MAX as f64;
/// Limits that keep the error-term computations free of overflow and underflow.
const SPLIT_MAX: f64 = pow2(995);
const ADD_MAX: f64 = pow2(1020);
const TINY: f64 = pow2(-960);
const SPLITTER: f64 = 134217729.0; // 2^27 + 1
const INF: f64 = f64::INFINITY;

fn ma(off: u32) -> MemArg {
    MemArg { offset: off as u64, align: 0, memory_index: 0 }
}

fn f(v: f64) -> W<'static> {
    W::F64Const(v.into())
}

// ---- instruction forms --------------------------------------------------------------

/// A memory operand's format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Src {
    M32,
    M64,
    M80,
    I16,
    I32,
    I64,
}

impl Src {
    fn of(s: MemorySize) -> Option<Src> {
        Some(match s {
            MemorySize::Float32 => Src::M32,
            MemorySize::Float64 => Src::M64,
            MemorySize::Float80 => Src::M80,
            MemorySize::Int16 => Src::I16,
            MemorySize::Int32 => Src::I32,
            MemorySize::Int64 => Src::I64,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

/// Operands of an arithmetic instruction: ST(0) op= memory, ST(0) op= ST(i), ST(i) op= ST(0).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ASrc {
    Mem(Src),
    St0Sti,
    StiSt0,
}

/// What ST(0) is compared with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CSrc {
    Mem(Src),
    Sti,
    Zero,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntMode {
    Fist,
    Fistp,
    Fisttp,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    Load(Src),
    LoadSt,
    Const(bool),
    /// fst/fstp to memory (pops).
    Store(Src, bool),
    StoreSt(bool),
    StoreInt(Src, IntMode),
    Rndint,
    Chs,
    Abs,
    Xch,
    /// operation, reversed, operands, pops
    Arith(Op, bool, ASrc, bool),
    /// compared with, EFLAGS result (fcomi), pops
    Cmp(CSrc, bool, u8),
    Fnstsw(bool),
    Fnstcw,
    Fldcw,
    Ffree,
    /// fincstp (true) / fdecstp
    Incstp(bool),
    Wait,
    Xam,
    /// fsin (true) / fcos
    Trig(bool),
}

/// Every form, in helper order (export `f<k>` is `FORMS[k]`).
pub static FORMS: LazyLock<Vec<Form>> = LazyLock::new(|| {
    use Form::*;
    let mut v = vec![];
    for s in [Src::M32, Src::M64, Src::M80, Src::I16, Src::I32, Src::I64] {
        v.push(Load(s));
    }
    v.extend([LoadSt, Const(true), Const(false)]);
    for (s, pop) in [(Src::M32, false), (Src::M64, false), (Src::M32, true), (Src::M64, true), (Src::M80, true)] {
        v.push(Store(s, pop));
    }
    v.extend([StoreSt(false), StoreSt(true)]);
    for s in [Src::I16, Src::I32, Src::I64] {
        for m in [IntMode::Fist, IntMode::Fistp, IntMode::Fisttp] {
            v.push(StoreInt(s, m));
        }
    }
    v.extend([Rndint, Chs, Abs, Xch]);
    for op in [Op::Add, Op::Sub, Op::Mul, Op::Div] {
        for rev in [false, true] {
            if rev && matches!(op, Op::Add | Op::Mul) {
                continue;
            }
            for s in [Src::M32, Src::M64, Src::I16, Src::I32] {
                v.push(Arith(op, rev, ASrc::Mem(s), false));
            }
            v.push(Arith(op, rev, ASrc::St0Sti, false));
            v.push(Arith(op, rev, ASrc::StiSt0, false));
            v.push(Arith(op, rev, ASrc::StiSt0, true));
        }
    }
    for eflags in [false, true] {
        for pops in 0..=2u8 {
            if eflags && pops == 2 {
                continue;
            }
            v.push(Cmp(CSrc::Sti, eflags, pops));
        }
    }
    for s in [Src::M32, Src::M64, Src::I16, Src::I32] {
        for pops in 0..=1u8 {
            v.push(Cmp(CSrc::Mem(s), false, pops));
        }
    }
    v.push(Cmp(CSrc::Zero, false, 0));
    v.extend([Fnstsw(false), Fnstsw(true), Fnstcw, Fldcw, Ffree, Incstp(true), Incstp(false), Wait]);
    v.extend([Xam, Trig(true), Trig(false)]);
    v
});

/// How the gate passes `a`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg {
    /// The memory operand's address.
    Mem,
    St(u32),
    None,
}

fn st_operand(instr: &Instruction, n: u32) -> Option<u32> {
    (instr.op_count() > n && instr.op_kind(n) == OpKind::Register && instr.op_register(n).is_st()).then(|| instr.op_register(n).number() as u32)
}

/// The helper for an instruction, if there is one: its index in `FORMS` and `a`.
pub fn form_of(instr: &Instruction) -> Option<(u32, Arg)> {
    use Mnemonic::*;
    let mn = instr.mnemonic();
    let mem = (0..instr.op_count()).any(|n| instr.op_kind(n) == OpKind::Memory);
    let src = if mem { Src::of(instr.memory_size()) } else { None };
    let op0 = st_operand(instr, 0);
    let op1 = st_operand(instr, 1);
    let (form, arg) = match mn {
        Fld | Fild if mem => (Form::Load(src?), Arg::Mem),
        Fld => (Form::LoadSt, Arg::St(op0?)),
        Fld1 | Fldz => (Form::Const(mn == Fld1), Arg::None),
        Fst | Fstp if mem => (Form::Store(src?, mn == Fstp), Arg::Mem),
        Fst | Fstp => (Form::StoreSt(mn == Fstp), Arg::St(op0?)),
        Fist | Fistp | Fisttp => {
            let m = match mn {
                Fist => IntMode::Fist,
                Fistp => IntMode::Fistp,
                _ => IntMode::Fisttp,
            };
            (Form::StoreInt(src?, m), Arg::Mem)
        }
        Frndint => (Form::Rndint, Arg::None),
        Fxam => (Form::Xam, Arg::None),
        Fsin => (Form::Trig(true), Arg::None),
        Fcos => (Form::Trig(false), Arg::None),
        Fchs => (Form::Chs, Arg::None),
        Fabs => (Form::Abs, Arg::None),
        Fxch => (Form::Xch, Arg::St(op1.unwrap_or(1))),
        Fadd | Fiadd | Faddp | Fsub | Fisub | Fsubp | Fsubr | Fisubr | Fsubrp | Fmul | Fimul | Fmulp | Fdiv | Fidiv | Fdivp | Fdivr
        | Fidivr | Fdivrp => {
            let op = match mn {
                Fadd | Fiadd | Faddp => Op::Add,
                Fsub | Fisub | Fsubp | Fsubr | Fisubr | Fsubrp => Op::Sub,
                Fmul | Fimul | Fmulp => Op::Mul,
                _ => Op::Div,
            };
            let rev = matches!(mn, Fsubr | Fisubr | Fsubrp | Fdivr | Fidivr | Fdivrp);
            let pop = matches!(mn, Faddp | Fsubp | Fsubrp | Fmulp | Fdivp | Fdivrp);
            // Destination ST(0) for memory forms and `st0, sti`; ST(i) for `sti, st0` and pops.
            if mem {
                (Form::Arith(op, rev, ASrc::Mem(src?), false), Arg::Mem)
            } else {
                let a = op0.unwrap_or(0);
                if a == 0 && !pop {
                    (Form::Arith(op, rev, ASrc::St0Sti, false), Arg::St(op1.unwrap_or(1)))
                } else {
                    (Form::Arith(op, rev, ASrc::StiSt0, pop), Arg::St(a))
                }
            }
        }
        Fcom | Fcomp | Ficom | Ficomp | Fucom | Fucomp if mem => {
            let pops = matches!(mn, Fcomp | Ficomp) as u8;
            (Form::Cmp(CSrc::Mem(src?), false, pops), Arg::Mem)
        }
        Fcom | Fcomp | Fucom | Fucomp | Fcompp | Fucompp | Fcomi | Fcomip | Fucomi | Fucomip => {
            let pops = match mn {
                Fcompp | Fucompp => 2,
                Fcomp | Fucomp | Fcomip | Fucomip => 1,
                _ => 0,
            };
            let eflags = matches!(mn, Fcomi | Fcomip | Fucomi | Fucomip);
            let other = if pops == 2 { 1 } else { op1.unwrap_or(1) };
            (Form::Cmp(CSrc::Sti, eflags, pops), Arg::St(other))
        }
        Ftst => (Form::Cmp(CSrc::Zero, false, 0), Arg::None),
        Fnstsw => (Form::Fnstsw(mem), if mem { Arg::Mem } else { Arg::None }),
        Fnstcw => (Form::Fnstcw, Arg::Mem),
        Fldcw => (Form::Fldcw, Arg::Mem),
        Ffree => (Form::Ffree, Arg::St(op0?)),
        Fincstp | Fdecstp => (Form::Incstp(mn == Fincstp), Arg::None),
        Wait => (Form::Wait, Arg::None),
        _ => return None,
    };
    let k = FORMS.iter().position(|&x| x == form)?;
    Some((k as u32, arg))
}

// ---- the helper module ----------------------------------------------------------------

/// Imported from the runtime (`rt`): fsin's and fcos's value for a double
/// (`fpu::sin`, `fpu::cos`).
const I_SIN: u32 = 0;
const I_COS: u32 = 1;
pub const RT_IMPORTS: [&str; 2] = ["sin", "cos"];
const N_IMPORTED: u32 = 2;
/// Internal math helpers, after the imports.
const H_ADD: u32 = N_IMPORTED;
const H_MUL: u32 = N_IMPORTED + 1;
const H_DIV: u32 = N_IMPORTED + 2;
const H_X80_LOAD: u32 = N_IMPORTED + 3;
const H_X80_STORE: u32 = N_IMPORTED + 4;
const H_I64_EXT: u32 = N_IMPORTED + 5;
const H_EXT_I64: u32 = N_IMPORTED + 6;
const N_INTERNAL: u32 = N_IMPORTED + 7;

/// Type of every form helper, in translated modules and here: (a, ip) -> i32.
pub fn form_type() -> ([ValType; 2], [ValType; 1]) {
    ([ValType::I32, ValType::I32], [ValType::I32])
}

pub fn export_name(k: usize) -> String {
    format!("f{k}")
}

/// The `x87` module: imports guest memory and `fs_base` from `cpu`, exports `f<k>`.
pub fn module(shared_memory: bool) -> Vec<u8> {
    let mut types = TypeSection::new();
    let (p, r) = form_type();
    types.ty().function(p, r); // 0: form
    types.ty().function([ValType::F64, ValType::F64, ValType::I32], [ValType::F64]); // 1: arith
    types.ty().function([ValType::I32], [ValType::F64]); // 2: x80 load
    types.ty().function([ValType::I32, ValType::F64], []); // 3: x80 store
    types.ty().function([ValType::I32, ValType::I64], []); // 4: i64 -> image
    types.ty().function([ValType::I32, ValType::I32], [ValType::I32]); // 5: image -> i64
    types.ty().function([ValType::F64], [ValType::F64]); // 6: sin, cos
    let mut imports = ImportSection::new();
    imports.import(
        abi::CPU_MODULE,
        abi::MEMORY_NAME,
        EntityType::Memory(MemoryType {
            minimum: 0,
            maximum: if shared_memory { Some(65536) } else { None },
            memory64: false,
            shared: shared_memory,
            page_size_log2: None,
        }),
    );
    let fs = abi::GLOBALS.iter().find(|g| g.0 == abi::Global::FsBase).unwrap();
    imports.import(abi::CPU_MODULE, fs.1, EntityType::Global(GlobalType { val_type: ValType::I32, mutable: true, shared: false }));
    for name in RT_IMPORTS {
        imports.import(abi::RT_MODULE, name, EntityType::Function(6));
    }
    let mut funcs = FunctionSection::new();
    let mut code = CodeSection::new();
    for (ty, body) in [(1, h_add()), (1, h_mul()), (1, h_div()), (2, h_x80_load()), (3, h_x80_store()), (4, h_i64_ext()), (5, h_ext_i64())] {
        funcs.function(ty);
        code.function(&body);
    }
    let mut exports = ExportSection::new();
    for (k, form) in FORMS.iter().enumerate() {
        funcs.function(0);
        code.function(&form_body(*form));
        exports.export(&export_name(k), ExportKind::Func, N_INTERNAL + k as u32);
    }
    let mut m = Module::new();
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    m.section(&exports);
    m.section(&code);
    m.finish()
}

// Locals of a form helper.
const A: u32 = 0; // address or ST index
const IP: u32 = 1;
const F: u32 = 2; // block address
const PA: u32 = 3; // physical register numbers
const PB: u32 = 4;
const XB: u32 = 5; // flag temporaries
const XC: u32 = 6;
const Q: u32 = 7; // i64
const X: u32 = 8; // f64
const Y: u32 = 9;
const R: u32 = 10;

/// Set in `ip` when the instruction's C1 is overwritten before anything reads the
/// status word (the next x87 instruction in its block writes it).
pub const IP_C1_DEAD: u32 = 0x8000_0000;

/// Builds one function body.
struct B(Function);

impl B {
    fn new(locals: Vec<(u32, ValType)>) -> Self {
        B(Function::new(locals))
    }
    fn form() -> Self {
        B::new(vec![(5, ValType::I32), (1, ValType::I64), (3, ValType::F64)])
    }
    /// The quick lane applies: C1 dead here, PE set already (sticky: an inexact result
    /// changes nothing), rounding to nearest (what f64 and f32 do).
    fn quick(&mut self) -> &mut Self {
        self.g(IP).c(31).i(W::I32ShrU);
        self.g(F).i(W::I32Load16U(ma(fb::SW))).c(SW_PE_SHIFT).i(W::I32ShrU).i(W::I32And);
        self.g(F).i(W::I32Load16U(ma(fb::CW))).c(0xC00).i(W::I32And).i(W::I32Eqz).i(W::I32And)
    }
    fn i(&mut self, ins: W) -> &mut Self {
        self.0.instruction(&ins);
        self
    }
    fn c(&mut self, v: u32) -> &mut Self {
        self.i(W::I32Const(v as i32))
    }
    fn g(&mut self, l: u32) -> &mut Self {
        self.i(W::LocalGet(l))
    }
    fn s(&mut self, l: u32) -> &mut Self {
        self.i(W::LocalSet(l))
    }
    fn t(&mut self, l: u32) -> &mut Self {
        self.i(W::LocalTee(l))
    }
    fn k(&mut self, v: f64) -> &mut Self {
        self.i(f(v))
    }
    fn finish(mut self) -> Function {
        self.0.instruction(&W::End);
        self.0
    }

    // -- math helper idioms --

    /// `|l| op v`
    fn abs_cmp(&mut self, l: u32, v: f64, op: W<'static>) -> &mut Self {
        self.g(l).i(W::F64Abs).k(v).i(op)
    }
    fn ret_nan(&mut self) -> &mut Self {
        self.k(f64::NAN).i(W::Return)
    }
    /// if (cond on stack) return NaN
    fn nan_if(&mut self) -> &mut Self {
        self.i(W::If(BlockType::Empty)).ret_nan().i(W::End)
    }
    /// sw = sw & !C1 at block address local `fl`
    fn clear_c1_at(&mut self, fl: u32) -> &mut Self {
        self.g(fl).g(fl).i(W::I32Load16U(ma(fb::SW))).c(!SW_C1 & 0xffff).i(W::I32And).i(W::I32Store16(ma(fb::SW)))
    }
    /// The result is exact when `cond` (on stack) holds: clear C1 and return `r`.
    /// Unless the precision is 53 bits (`FAST_ARITH`), only an exact result (`err` 0)
    /// is decided here.
    fn exact_unless_pc53(&mut self, fl: u32, err: u32) -> &mut Self {
        self.g(fl).i(W::I32Load(ma(fb::FAST))).c(fb::FAST_ARITH).i(W::I32And).i(W::I32Eqz);
        self.g(err).k(0.0).i(W::F64Ne).i(W::I32And).nan_if()
    }
    fn exact_if(&mut self, fl: u32, r: u32) -> &mut Self {
        self.i(W::If(BlockType::Empty)).clear_c1_at(fl).g(r).i(W::Return).i(W::End)
    }
    /// PE if `err` != 0, and C1 if the result `r` was rounded away from zero, which is
    /// when `err` (exact - r, or a value with its sign) and `r` differ in sign.
    fn flags_from_error(&mut self, fl: u32, err: u32, r: u32) -> &mut Self {
        self.g(fl).g(fl).i(W::I32Load16U(ma(fb::SW))).c(!SW_C1 & 0xffff).i(W::I32And);
        self.g(err).k(0.0).i(W::F64Ne).c(SW_PE_SHIFT).i(W::I32Shl).i(W::I32Or);
        self.g(err).k(0.0).i(W::F64Lt).g(r).k(0.0).i(W::F64Lt).i(W::I32Xor);
        self.g(err).k(0.0).i(W::F64Ne).i(W::I32And).c(9).i(W::I32Shl).i(W::I32Or);
        self.i(W::I32Store16(ma(fb::SW)))
    }
    /// `into` = the control word's rounding control (block address local `fl`), also
    /// left on the stack.
    fn rc_at(&mut self, fl: u32, into: u32) -> &mut Self {
        self.g(fl).i(W::I32Load16U(ma(fb::CW))).c(10).i(W::I32ShrU).c(3).i(W::I32And).t(into)
    }
    /// i32 on the stack: 1 (a step away from zero) -> +1, 0 (toward zero) -> -1.
    fn step(&mut self) -> &mut Self {
        self.c(1).i(W::I32Shl).c(1).i(W::I32Sub)
    }
    /// Rounds `r`, the round-to-nearest result of an operation whose exact value is
    /// r + e (`e` exact, or with the sign of the true error), by the control word's
    /// RC: in a directed mode, one step toward the exact value when it lies on the
    /// wrong side (chop: nearer zero, up: above, down: below). `e` then gets the sign
    /// of the new error. Returns NaN when the step overflows.
    fn round_directed(&mut self, fl: u32, r: u32, e: u32, rc: u32) -> &mut Self {
        self.rc_at(fl, rc).i(W::If(BlockType::Empty));
        self.g(e).k(0.0).i(W::F64Lt).g(r).k(0.0).i(W::F64Lt).i(W::I32Xor).g(e).k(0.0).i(W::F64Ne).i(W::I32And);
        self.g(e).k(0.0).i(W::F64Gt).g(e).k(0.0).i(W::F64Lt).g(rc).c(2).i(W::I32Eq).i(W::Select);
        self.g(rc).c(3).i(W::I32Eq).i(W::Select).i(W::If(BlockType::Empty));
        // the exact value lies beyond r (away from zero) when e and r agree in sign
        self.g(r).i(W::I64ReinterpretF64);
        self.g(e).k(0.0).i(W::F64Lt).g(r).k(0.0).i(W::F64Lt).i(W::I32Eq).step().i(W::I64ExtendI32S);
        self.i(W::I64Add).i(W::F64ReinterpretI64).s(r);
        self.g(e).i(W::F64Neg).s(e);
        self.abs_cmp(r, INF, W::F64Eq).nan_if();
        self.i(W::End).i(W::End)
    }
    /// Dekker's split: hi, lo = x
    fn split(&mut self, x: u32, c: u32, hi: u32, lo: u32) -> &mut Self {
        self.k(SPLITTER).g(x).i(W::F64Mul).s(c);
        self.g(c).g(c).g(x).i(W::F64Sub).i(W::F64Sub).s(hi);
        self.g(x).g(hi).i(W::F64Sub).s(lo)
    }
    /// err = exact x*y - p, from the splits of x and y
    fn product_error(&mut self, p: u32, xh: u32, xl: u32, yh: u32, yl: u32, err: u32) -> &mut Self {
        self.g(xh).g(yh).i(W::F64Mul).g(p).i(W::F64Sub);
        self.g(xh).g(yl).i(W::F64Mul).i(W::F64Add);
        self.g(xl).g(yh).i(W::F64Mul).i(W::F64Add);
        self.g(xl).g(yl).i(W::F64Mul).i(W::F64Add).s(err)
    }

    // -- form helper idioms (locals A.. Y) --

    /// if (cond on stack) return -1 (the caller runs rt.fpu)
    fn fail_if(&mut self) -> &mut Self {
        self.i(W::If(BlockType::Empty)).c(u32::MAX).i(W::Return).i(W::End)
    }
    fn fblock(&mut self) -> &mut Self {
        self.i(W::GlobalGet(0)).c(fb::OFFSET).i(W::I32Sub).s(F)
    }
    fn top(&mut self) -> &mut Self {
        self.g(F).i(W::I32Load(ma(fb::TOP)))
    }
    /// `l = (TOP + delta) & 7`
    fn phys(&mut self, l: u32, delta: u32) -> &mut Self {
        self.top();
        if delta != 0 {
            self.c(delta).i(W::I32Add);
        }
        self.c(7).i(W::I32And).s(l)
    }
    /// `l = (TOP + A) & 7` (A = ST index argument)
    fn phys_a(&mut self, l: u32) -> &mut Self {
        self.top().g(A).i(W::I32Add).c(7).i(W::I32And).s(l)
    }
    fn kind(&mut self, l: u32) -> &mut Self {
        self.g(F).g(l).i(W::I32Add).i(W::I32Load8U(ma(fb::KIND)))
    }
    fn set_kind(&mut self, l: u32, k: u8) -> &mut Self {
        self.g(F).g(l).i(W::I32Add).c(k as u32).i(W::I32Store8(ma(fb::KIND)))
    }
    /// Falls back unless register `l` holds an f64.
    fn need_f64(&mut self, l: u32) -> &mut Self {
        self.kind(l).c(fb::VALID as u32).i(W::I32Ne).fail_if()
    }
    /// Falls back unless register `l` is empty (a push would overflow otherwise).
    fn need_empty(&mut self, l: u32) -> &mut Self {
        self.kind(l).c(fb::VALID as u32).i(W::I32And).fail_if()
    }
    fn need_fast(&mut self, bit: u32) -> &mut Self {
        self.g(F).i(W::I32Load(ma(fb::FAST))).c(bit).i(W::I32And).i(W::I32Eqz).fail_if()
    }
    fn reg_addr(&mut self, l: u32) -> &mut Self {
        self.g(F).g(l).c(3).i(W::I32Shl).i(W::I32Add)
    }
    fn ext_addr(&mut self, l: u32) -> &mut Self {
        self.g(F).c(fb::EXT).i(W::I32Add).g(l).c(10).i(W::I32Mul).i(W::I32Add)
    }
    fn load_st(&mut self, l: u32, into: u32) -> &mut Self {
        self.reg_addr(l).i(W::F64Load(ma(fb::REGS))).s(into)
    }
    fn store_st(&mut self, l: u32, from: u32) -> &mut Self {
        self.reg_addr(l).g(from).i(W::F64Store(ma(fb::REGS)))
    }
    fn set_top(&mut self, l: u32) -> &mut Self {
        self.g(F).g(l).i(W::I32Store(ma(fb::TOP)))
    }
    /// Marks ST(0) empty (keeping where its image is) and increments TOP.
    fn pop_st(&mut self) -> &mut Self {
        self.phys(PB, 0);
        self.g(F).g(PB).i(W::I32Add).kind(PB).c(!(fb::VALID as u32) & 0xff).i(W::I32And).i(W::I32Store8(ma(fb::KIND)));
        self.g(PB).c(1).i(W::I32Add).c(7).i(W::I32And).s(PB).set_top(PB)
    }
    /// C1 = 0, unless the next x87 instruction overwrites it anyway (`IP_C1_DEAD`).
    fn clear_c1(&mut self) -> &mut Self {
        self.g(IP).c(0).i(W::I32GeS).i(W::If(BlockType::Empty));
        self.clear_c1_at(F);
        self.i(W::End)
    }
    /// PE (sticky) and C1 from XB (inexact) and XC (rounded up in magnitude).
    fn sw_inexact(&mut self) -> &mut Self {
        self.g(F).g(F).i(W::I32Load16U(ma(fb::SW))).c(!SW_C1 & 0xffff).i(W::I32And);
        self.g(XB).c(SW_PE_SHIFT).i(W::I32Shl).g(XC).c(9).i(W::I32Shl).i(W::I32Or).i(W::I32Or);
        self.i(W::I32Store16(ma(fb::SW)))
    }
    /// `into` = the control word's rounding control, also left on the stack.
    fn rc(&mut self, into: u32) -> &mut Self {
        self.rc_at(F, into)
    }
    /// FIP = this instruction, unless the next x87 instruction (which writes C1 and
    /// FIP alike) comes before anything can read it (`IP_C1_DEAD`).
    fn set_fip(&mut self) -> &mut Self {
        self.g(IP).c(0).i(W::I32GeS).i(W::If(BlockType::Empty));
        self.g(F).g(IP).i(W::I32Store(ma(fb::FIP)));
        self.i(W::End)
    }
    /// XB = (y != x), XC = XB && |y| > |x|: inexact, and rounded up in magnitude.
    fn inexact_flags(&mut self, x: u32, y: u32) -> &mut Self {
        self.g(y).g(x).i(W::F64Ne).t(XB);
        self.g(y).i(W::F64Abs).g(x).i(W::F64Abs).i(W::F64Gt).i(W::I32And).s(XC)
    }
    /// Pushes the f64 in `from` (stack overflow falls back).
    fn push_value(&mut self, from: u32) -> &mut Self {
        self.phys(PA, 7).need_empty(PA).store_st(PA, from).set_kind(PA, fb::VALID).set_top(PA)
    }
    /// Loads the memory operand at A as a register value into `into`, falling back on
    /// NaNs and denormals (they raise IE/DE or change the register class) and on 64-bit
    /// integers that are not doubles.
    fn mem_value(&mut self, src: Src, into: u32) -> &mut Self {
        self.g(A);
        match src {
            Src::M32 | Src::M64 => {
                if src == Src::M32 {
                    self.i(W::F32Load(ma(0))).i(W::F64PromoteF32);
                } else {
                    self.i(W::F64Load(ma(0)));
                }
                self.s(into);
                // ok = |x| >= smallest normal (false for NaN) or x == 0
                self.abs_cmp(into, if src == Src::M32 { F32_MIN_NORMAL } else { MIN_NORMAL }, W::F64Ge);
                self.g(into).k(0.0).i(W::F64Eq).i(W::I32Or).i(W::I32Eqz).fail_if();
            }
            Src::I16 => {
                self.i(W::I32Load16S(ma(0))).i(W::F64ConvertI32S).s(into);
            }
            Src::I32 => {
                self.i(W::I32Load(ma(0))).i(W::F64ConvertI32S).s(into);
            }
            Src::I64 => {
                self.i(W::I64Load(ma(0))).t(Q).i(W::F64ConvertI64S).s(into);
                self.exact_i64(into).i(W::I32Eqz).fail_if();
            }
            Src::M80 => unreachable!("80-bit loads have their own path"),
        }
        self
    }
    /// Pushes 1 when the f64 `x` converted from Q equals it.
    fn exact_i64(&mut self, x: u32) -> &mut Self {
        self.g(x).k(9223372036854775808.0).i(W::F64Lt).g(x).i(W::I64TruncSatF64S).g(Q).i(W::I64Eq).i(W::I32And)
    }
    /// `into` = X rounded to an integer by the control word's RC (or toward zero).
    fn round_rc(&mut self, into: u32, truncate: bool) -> &mut Self {
        if truncate {
            return self.g(X).i(W::F64Trunc).s(into);
        }
        // select(a, b, c) = c ? a : b
        self.rc(XB).i(W::Drop);
        self.g(X).i(W::F64Nearest).g(X).i(W::F64Floor).g(X).i(W::F64Ceil).g(X).i(W::F64Trunc);
        self.g(XB).c(2).i(W::I32Eq).i(W::Select);
        self.g(XB).c(1).i(W::I32Eq).i(W::Select);
        self.g(XB).i(W::I32Eqz).i(W::Select).s(into)
    }
    fn done(&mut self, result: Option<u32>) -> &mut Self {
        match result {
            Some(l) => self.g(l),
            None => self.c(0),
        }
    }
}

/// The helper for one form.
fn form_body(form: Form) -> Function {
    use Form::*;
    let mut b = B::form();
    b.fblock();
    let mut result = None;
    match form {
        Load(Src::M80) => {
            b.need_fast(fb::FAST_MOVE).phys(PA, 7).need_empty(PA);
            b.g(A).i(W::Call(H_X80_LOAD)).t(X).g(X).i(W::F64Eq).i(W::If(BlockType::Empty));
            b.store_st(PA, X).set_kind(PA, fb::VALID);
            b.i(W::Else);
            // not a double: keep the 80-bit image
            b.ext_addr(PA).g(A).i(W::I64Load(ma(0))).i(W::I64Store(ma(0)));
            b.ext_addr(PA).g(A).i(W::I32Load16U(ma(8))).i(W::I32Store16(ma(8)));
            b.set_kind(PA, fb::VALID | fb::EXT_IMAGE);
            b.i(W::End);
            b.set_top(PA).clear_c1().set_fip();
        }
        Load(Src::I64) => {
            // exact as a double, or else kept as its (exact) 80-bit image
            b.need_fast(fb::FAST_MOVE).phys(PA, 7).need_empty(PA);
            b.g(A).i(W::I64Load(ma(0))).t(Q).i(W::F64ConvertI64S).s(X);
            b.exact_i64(X).i(W::If(BlockType::Empty));
            b.store_st(PA, X).set_kind(PA, fb::VALID);
            b.i(W::Else);
            b.ext_addr(PA).g(Q).i(W::Call(H_I64_EXT)).set_kind(PA, fb::VALID | fb::EXT_IMAGE);
            b.i(W::End);
            b.set_top(PA).clear_c1().set_fip();
        }
        Load(s) => {
            b.need_fast(fb::FAST_MOVE).mem_value(s, X).push_value(X).clear_c1().set_fip();
        }
        LoadSt => {
            b.need_fast(fb::FAST_MOVE).phys_a(PB).need_f64(PB).load_st(PB, X).push_value(X).clear_c1().set_fip();
        }
        Const(one) => {
            b.need_fast(fb::FAST_MOVE).k(if one { 1.0 } else { 0.0 }).s(X).push_value(X).clear_c1().set_fip();
        }
        Store(s, pop) => {
            b.phys(PA, 0);
            match s {
                Src::M64 => {
                    b.need_fast(fb::FAST_MOVE).need_f64(PA);
                    b.g(A).reg_addr(PA).i(W::F64Load(ma(fb::REGS))).i(W::F64Store(ma(0)));
                    b.clear_c1();
                }
                Src::M32 => {
                    b.need_fast(fb::FAST_INEXACT).need_f64(PA).load_st(PA, X);
                    // beyond the largest single (overflow in some rounding mode)
                    b.abs_cmp(X, F32_MAX, W::F64Gt).abs_cmp(X, INF, W::F64Ne).i(W::I32And).fail_if();
                    // near or below the single-precision underflow threshold
                    b.abs_cmp(X, F32_TINY, W::F64Lt).g(X).k(0.0).i(W::F64Ne).i(W::I32And).fail_if();
                    // quick: rounded to nearest as wasm does; PE stays set, C1 is dead
                    b.quick().i(W::If(BlockType::Empty));
                    b.g(A).g(X).i(W::F32DemoteF64).i(W::F32Store(ma(0)));
                    b.i(W::Else);
                    b.g(X).i(W::F32DemoteF64).i(W::F64PromoteF32).s(Y);
                    // directed rounding: one step toward X when Y lies on the wrong side
                    // (chop: beyond it in magnitude, up: below it, down: above it)
                    b.rc(XB).i(W::If(BlockType::Empty));
                    b.g(Y).i(W::F64Abs).g(X).i(W::F64Abs).i(W::F64Gt);
                    b.g(Y).g(X).i(W::F64Lt).g(Y).g(X).i(W::F64Gt).g(XB).c(2).i(W::I32Eq).i(W::Select);
                    b.g(XB).c(3).i(W::I32Eq).i(W::Select).i(W::If(BlockType::Empty));
                    b.g(Y).i(W::F32DemoteF64).i(W::I32ReinterpretF32);
                    b.g(Y).i(W::F64Abs).g(X).i(W::F64Abs).i(W::F64Lt).step();
                    b.i(W::I32Add).i(W::F32ReinterpretI32).i(W::F64PromoteF32).s(Y);
                    b.i(W::End).i(W::End);
                    b.inexact_flags(X, Y);
                    b.g(A).g(Y).i(W::F32DemoteF64).i(W::F32Store(ma(0)));
                    b.sw_inexact();
                    b.i(W::End);
                }
                _ => {
                    // Float80: the double's image, or the register's own image
                    b.need_fast(fb::FAST_MOVE);
                    b.kind(PA).c(fb::VALID as u32).i(W::I32And).i(W::I32Eqz).fail_if();
                    b.kind(PA).c(fb::VALID as u32).i(W::I32Eq).i(W::If(BlockType::Empty));
                    b.g(A).reg_addr(PA).i(W::F64Load(ma(fb::REGS))).i(W::Call(H_X80_STORE));
                    b.i(W::Else);
                    b.g(A).ext_addr(PA).i(W::I64Load(ma(0))).i(W::I64Store(ma(0)));
                    b.g(A).ext_addr(PA).i(W::I32Load16U(ma(8))).i(W::I32Store16(ma(8)));
                    b.i(W::End);
                    b.clear_c1();
                }
            }
            if pop {
                b.pop_st();
            }
            b.set_fip();
        }
        StoreSt(pop) => {
            b.need_fast(fb::FAST_MOVE).phys(PA, 0).need_f64(PA).phys_a(PB);
            b.load_st(PA, X).store_st(PB, X).set_kind(PB, fb::VALID).clear_c1();
            if pop {
                b.pop_st();
            }
            b.set_fip();
        }
        StoreInt(s, mode) => {
            b.need_fast(fb::FAST_INEXACT).phys(PA, 0);
            if s == Src::I64 {
                // an 80-bit image holding an exact integer (fild m64 of a large value)
                b.kind(PA).c((fb::VALID | fb::EXT_IMAGE) as u32).i(W::I32Eq).i(W::If(BlockType::Empty));
                b.ext_addr(PA).g(A).i(W::Call(H_EXT_I64)).i(W::I32Eqz).fail_if();
                b.clear_c1();
                if mode != IntMode::Fist {
                    b.pop_st();
                }
                b.set_fip().c(0).i(W::Return);
                b.i(W::End);
            }
            b.need_f64(PA).load_st(PA, X).round_rc(Y, mode == IntMode::Fisttp);
            let (lo, hi, hi_inclusive) = match s {
                Src::I16 => (-32768.0, 32767.0, true),
                Src::I32 => (-2147483648.0, 2147483647.0, true),
                _ => (-9223372036854775808.0, 9223372036854775808.0, false),
            };
            // out of range (infinities included): IE and the integer indefinite
            b.g(Y).k(lo).i(W::F64Ge).g(Y).k(hi).i(if hi_inclusive { W::F64Le } else { W::F64Lt }).i(W::I32And).i(W::I32Eqz).fail_if();
            b.inexact_flags(X, Y);
            b.g(A).g(Y);
            match s {
                Src::I16 => b.i(W::I32TruncF64S).i(W::I32Store16(ma(0))),
                Src::I32 => b.i(W::I32TruncF64S).i(W::I32Store(ma(0))),
                _ => b.i(W::I64TruncF64S).i(W::I64Store(ma(0))),
            };
            b.sw_inexact();
            if mode != IntMode::Fist {
                b.pop_st();
            }
            b.set_fip();
        }
        Rndint => {
            b.need_fast(fb::FAST_INEXACT).phys(PA, 0).need_f64(PA).load_st(PA, X).round_rc(Y, false);
            b.inexact_flags(X, Y).store_st(PA, Y).sw_inexact().set_fip();
        }
        Chs | Abs => {
            b.need_fast(fb::FAST_MOVE).phys(PA, 0).need_f64(PA).load_st(PA, X);
            b.g(X).i(if form == Chs { W::F64Neg } else { W::F64Abs }).s(X);
            b.store_st(PA, X).clear_c1().set_fip();
        }
        Xch => {
            b.need_fast(fb::FAST_MOVE).phys(PA, 0).phys_a(PB).need_f64(PA).need_f64(PB);
            b.load_st(PA, X).load_st(PB, Y).store_st(PA, Y).store_st(PB, X).clear_c1().set_fip();
        }
        Arith(op, reversed, src, pop) => {
            // (64-bit precision: the helpers take exact results only)
            b.need_fast(fb::FAST_ARITH | fb::FAST_EXACT64);
            match src {
                ASrc::Mem(s) => {
                    b.phys(PA, 0).need_f64(PA).mem_value(s, Y);
                }
                ASrc::St0Sti => {
                    b.phys(PA, 0).need_f64(PA).phys_a(PB).need_f64(PB).load_st(PB, Y);
                }
                ASrc::StiSt0 => {
                    b.phys_a(PA).need_f64(PA).phys(PB, 0).need_f64(PB).load_st(PB, Y);
                }
            }
            b.load_st(PA, X);
            let (x, y) = if reversed { (Y, X) } else { (X, Y) };
            // quick (precision 53 bits): the f64 result when it is a normal number (or an
            // exact zero sum), with PE set already and C1 dead
            b.g(F).i(W::I32Load(ma(fb::FAST))).c(fb::FAST_ARITH).i(W::I32And).c(0).i(W::I32Ne).quick().i(W::I32And).i(W::If(BlockType::Empty));
            b.g(x).g(y).i(match op {
                Op::Add => W::F64Add,
                Op::Sub => W::F64Sub,
                Op::Mul => W::F64Mul,
                Op::Div => W::F64Div,
            });
            b.s(R);
            b.abs_cmp(R, MIN_NORMAL, W::F64Ge).abs_cmp(R, INF, W::F64Lt).i(W::I32And);
            if matches!(op, Op::Add | Op::Sub) {
                b.g(R).k(0.0).i(W::F64Eq).i(W::I32Or);
            }
            b.i(W::If(BlockType::Empty));
            b.store_st(PA, R);
            if pop {
                b.pop_st();
            }
            b.set_fip().c(0).i(W::Return);
            b.i(W::End).i(W::End);
            b.g(x).g(y);
            if op == Op::Sub {
                b.i(W::F64Neg);
            }
            b.g(F).i(W::Call(match op {
                Op::Add | Op::Sub => H_ADD,
                Op::Mul => H_MUL,
                Op::Div => H_DIV,
            }));
            b.t(X).g(X).i(W::F64Ne).fail_if();
            b.store_st(PA, X);
            if pop {
                b.pop_st();
            }
            b.set_fip();
        }
        Cmp(src, eflags, pops) => {
            b.need_fast(fb::FAST_MOVE).phys(PA, 0).need_f64(PA);
            match src {
                CSrc::Mem(s) => {
                    b.mem_value(s, Y);
                }
                CSrc::Zero => {
                    b.k(0.0).s(Y);
                }
                CSrc::Sti => {
                    b.phys_a(PB).need_f64(PB).load_st(PB, Y);
                }
            }
            b.load_st(PA, X);
            // XB = st0 < other, XC = equal (no NaNs here)
            b.g(X).g(Y).i(W::F64Lt).s(XB);
            b.g(X).g(Y).i(W::F64Eq).s(XC);
            if !eflags {
                b.g(F).g(F).i(W::I32Load16U(ma(fb::SW))).c(!(SW_CC | SW_C1) & 0xffff).i(W::I32And);
                b.g(XB).c(8).i(W::I32Shl).g(XC).c(14).i(W::I32Shl).i(W::I32Or).i(W::I32Or);
                b.i(W::I32Store16(ma(fb::SW)));
            }
            for _ in 0..pops {
                b.pop_st();
            }
            b.set_fip();
            if eflags {
                // (fcomi leaves the condition codes, C1 too, as they are)
                // CF = below, ZF = equal, PF = 0 (unordered is not possible here)
                b.g(XB).g(XC).c(6).i(W::I32Shl).i(W::I32Or).s(XB);
                result = Some(XB);
            }
        }
        Fnstsw(mem) => {
            b.g(F).i(W::I32Load16U(ma(fb::SW))).top().c(11).i(W::I32Shl).i(W::I32Or).s(XB);
            if mem {
                b.g(A).g(XB).i(W::I32Store16(ma(0)));
            }
            result = Some(XB);
        }
        Fnstcw => {
            b.g(A).g(F).i(W::I32Load16U(ma(fb::CW))).i(W::I32Store16(ma(0)));
        }
        Fldcw => {
            b.need_fast(fb::FAST_MOVE);
            b.g(A).i(W::I32Load16U(ma(0))).c(0x1f7f).i(W::I32And).c(0x40).i(W::I32Or).s(XB);
            // unmasking an exception that is already flagged makes it pending
            b.g(F).i(W::I32Load16U(ma(fb::SW))).g(XB).c(u32::MAX).i(W::I32Xor).i(W::I32And).c(0x3f).i(W::I32And).fail_if();
            b.g(F).g(XB).i(W::I32Store16(ma(fb::CW)));
            // FAST bits as fpu::block::fast_bits: XC = PM
            b.g(XB).c(5).i(W::I32ShrU).c(1).i(W::I32And).s(XC);
            b.g(F).c(fb::FAST_MOVE).g(XC).c(fb::FAST_INEXACT.trailing_zeros()).i(W::I32Shl).i(W::I32Or);
            b.g(XC).g(XB).c(0x300).i(W::I32And).c(0x200).i(W::I32Eq).i(W::I32And).c(fb::FAST_ARITH.trailing_zeros()).i(W::I32Shl).i(W::I32Or);
            b.g(XB).c(0x300).i(W::I32And).c(0x300).i(W::I32Eq).c(fb::FAST_EXACT64.trailing_zeros()).i(W::I32Shl).i(W::I32Or);
            b.i(W::I32Store(ma(fb::FAST)));
        }
        Ffree => {
            b.need_fast(fb::FAST_MOVE).phys_a(PA);
            b.g(F).g(PA).i(W::I32Add).kind(PA).c(!(fb::VALID as u32) & 0xff).i(W::I32And).i(W::I32Store8(ma(fb::KIND)));
            b.clear_c1();
        }
        Incstp(inc) => {
            b.need_fast(fb::FAST_MOVE).phys(PA, if inc { 1 } else { 7 }).set_top(PA).clear_c1();
        }
        Wait => {
            b.need_fast(fb::FAST_MOVE);
        }
        Xam => {
            // C3 C2 C0: zero 1 0 0, normal 0 1 0, infinity 0 1 1; C1 the sign
            b.need_fast(fb::FAST_MOVE).phys(PA, 0).need_f64(PA).load_st(PA, X);
            b.g(X).k(0.0).i(W::F64Eq).s(XB);
            b.g(X).i(W::F64Abs).k(INF).i(W::F64Eq).s(XC);
            b.g(F).g(F).i(W::I32Load16U(ma(fb::SW))).c(!SW_CC & 0xffff).i(W::I32And);
            b.g(XB).c(14).i(W::I32Shl).i(W::I32Or);
            b.g(XB).i(W::I32Eqz).c(10).i(W::I32Shl).i(W::I32Or);
            b.g(XC).c(8).i(W::I32Shl).i(W::I32Or);
            b.g(X).i(W::I64ReinterpretF64).c(63).i(W::I64ExtendI32U).i(W::I64ShrU).i(W::I32WrapI64).c(9).i(W::I32Shl).i(W::I32Or);
            b.i(W::I32Store16(ma(fb::SW))).set_fip();
        }
        Trig(sin) => {
            // as rt.fpu: ST(0) = the double's sin/cos; C1 = C2 = 0; PE unless it is 0.
            // Infinities, |x| >= 2^63 (C2: out of range) and denormal results fall back.
            b.need_fast(fb::FAST_INEXACT).phys(PA, 0).need_f64(PA).load_st(PA, X);
            b.g(X).i(W::F64Abs).k(pow2(63)).i(W::F64Ge).fail_if();
            b.g(X).k(0.0).i(W::F64Ne).s(XB);
            if sin {
                b.g(X).i(W::Call(I_SIN)).g(X).g(XB).i(W::Select).s(Y);
            } else {
                b.g(X).i(W::Call(I_COS)).k(1.0).g(XB).i(W::Select).s(Y);
            }
            b.g(Y).i(W::F64Abs).k(MIN_NORMAL).i(W::F64Lt).g(Y).k(0.0).i(W::F64Ne).i(W::I32And).fail_if();
            b.store_st(PA, Y);
            b.g(F).g(F).i(W::I32Load16U(ma(fb::SW))).c(!(SW_C1 | 0x400) & 0xffff).i(W::I32And);
            b.g(XB).c(SW_PE_SHIFT).i(W::I32Shl).i(W::I32Or).i(W::I32Store16(ma(fb::SW))).set_fip();
        }
    }
    b.done(result);
    b.finish()
}

/// (a, b, block) -> a + b, or NaN when the fast path must not decide.
fn h_add() -> Function {
    let (a, b, fl, s, bb, e, rc) = (0, 1, 2, 3, 4, 5, 6);
    let mut x = B::new(vec![(3, ValType::F64), (1, ValType::I32)]);
    x.g(a).g(b).i(W::F64Add).s(s);
    x.g(s).g(s).i(W::F64Ne).nan_if(); // inf - inf
    x.abs_cmp(s, INF, W::F64Eq).i(W::If(BlockType::Empty));
    x.abs_cmp(a, INF, W::F64Eq).abs_cmp(b, INF, W::F64Eq).i(W::I32Or).exact_if(fl, s);
    x.ret_nan().i(W::End); // overflow
    x.abs_cmp(a, ADD_MAX, W::F64Ge).abs_cmp(b, ADD_MAX, W::F64Ge).i(W::I32Or);
    // a nonzero result at or below the smallest normal may have been rounded as a denormal
    x.abs_cmp(s, MIN_NORMAL, W::F64Le).g(s).k(0.0).i(W::F64Ne).i(W::I32And).i(W::I32Or).nan_if();
    // TwoSum
    x.g(s).g(a).i(W::F64Sub).s(bb);
    x.g(a).g(s).g(bb).i(W::F64Sub).i(W::F64Sub).g(b).g(bb).i(W::F64Sub).i(W::F64Add).s(e);
    x.exact_unless_pc53(fl, e);
    x.round_directed(fl, s, e, rc);
    // an exact zero sum of opposite signs is -0 when rounding down
    x.g(rc).c(1).i(W::I32Eq).g(s).k(0.0).i(W::F64Eq).i(W::I32And).nan_if();
    x.flags_from_error(fl, e, s);
    x.g(s);
    x.finish()
}

/// (a, b, block) -> a * b, or NaN.
fn h_mul() -> Function {
    let (a, b, fl, p, c, ah, al, bh, bl, e, rc) = (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
    let mut x = B::new(vec![(7, ValType::F64), (1, ValType::I32)]);
    x.g(a).g(b).i(W::F64Mul).s(p);
    x.g(p).g(p).i(W::F64Ne).nan_if(); // 0 * inf
    x.abs_cmp(p, INF, W::F64Eq).i(W::If(BlockType::Empty));
    x.abs_cmp(a, INF, W::F64Eq).abs_cmp(b, INF, W::F64Eq).i(W::I32Or).exact_if(fl, p);
    x.ret_nan().i(W::End);
    x.g(p).k(0.0).i(W::F64Eq).i(W::If(BlockType::Empty));
    x.g(a).k(0.0).i(W::F64Eq).g(b).k(0.0).i(W::F64Eq).i(W::I32Or).exact_if(fl, p);
    x.ret_nan().i(W::End); // underflow
    x.abs_cmp(p, TINY, W::F64Lt).abs_cmp(a, SPLIT_MAX, W::F64Ge).i(W::I32Or).abs_cmp(b, SPLIT_MAX, W::F64Ge).i(W::I32Or).nan_if();
    x.split(a, c, ah, al).split(b, c, bh, bl).product_error(p, ah, al, bh, bl, e);
    x.exact_unless_pc53(fl, e);
    x.round_directed(fl, p, e, rc);
    x.flags_from_error(fl, e, p);
    x.g(p);
    x.finish()
}

/// (a, b, block) -> a / b, or NaN.
fn h_div() -> Function {
    let (a, b, fl, q, c, qh, ql, bh, bl, p, e, r, rc) = (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
    let mut x = B::new(vec![(9, ValType::F64), (1, ValType::I32)]);
    x.g(b).k(0.0).i(W::F64Eq).nan_if(); // zero divide, 0/0
    x.g(a).g(b).i(W::F64Div).s(q);
    x.g(q).g(q).i(W::F64Ne).nan_if(); // inf/inf
    x.abs_cmp(q, INF, W::F64Eq).i(W::If(BlockType::Empty));
    x.abs_cmp(a, INF, W::F64Eq).exact_if(fl, q);
    x.ret_nan().i(W::End);
    x.g(q).k(0.0).i(W::F64Eq).i(W::If(BlockType::Empty));
    x.g(a).k(0.0).i(W::F64Eq).abs_cmp(b, INF, W::F64Eq).i(W::I32Or).exact_if(fl, q);
    x.ret_nan().i(W::End);
    x.abs_cmp(q, TINY, W::F64Lt).abs_cmp(a, TINY, W::F64Lt).i(W::I32Or);
    x.abs_cmp(q, SPLIT_MAX, W::F64Ge).i(W::I32Or).abs_cmp(b, SPLIT_MAX, W::F64Ge).i(W::I32Or).abs_cmp(a, ADD_MAX, W::F64Ge).i(W::I32Or).nan_if();
    // remainder r = a - q*b, exact
    x.g(q).g(b).i(W::F64Mul).s(p);
    x.split(q, c, qh, ql).split(b, c, bh, bl).product_error(p, qh, ql, bh, bl, e);
    x.g(a).g(p).i(W::F64Sub).g(e).i(W::F64Sub).s(r);
    // a/b - q = r/b: e takes its sign
    x.g(r).i(W::F64Neg).g(r).g(b).k(0.0).i(W::F64Lt).i(W::Select).s(e);
    x.exact_unless_pc53(fl, e);
    x.round_directed(fl, q, e, rc);
    x.flags_from_error(fl, e, q);
    x.g(q);
    x.finish()
}

/// (address) -> the 80-bit value there as f64, or NaN when it is not a normal double,
/// zero or infinity.
fn h_x80_load() -> Function {
    let (addr, mant, se, e) = (0, 1, 2, 3);
    let mut x = B::new(vec![(1, ValType::I64), (2, ValType::I32)]);
    x.g(addr).i(W::I64Load(ma(0))).s(mant);
    x.g(addr).i(W::I32Load16U(ma(8))).s(se);
    // zero
    x.g(se).c(0x7fff).i(W::I32And).i(W::I32Eqz).g(mant).i(W::I64Eqz).i(W::I32And).i(W::If(BlockType::Empty));
    x.g(se).c(15).i(W::I32ShrU).i(W::I64ExtendI32U).i(W::I64Const(63)).i(W::I64Shl).i(W::F64ReinterpretI64).i(W::Return);
    x.i(W::End);
    // infinity
    x.g(se).c(0x7fff).i(W::I32And).c(0x7fff).i(W::I32Eq).g(mant).i(W::I64Const(i64::MIN)).i(W::I64Eq).i(W::I32And);
    x.i(W::If(BlockType::Empty));
    x.k(INF).k(-INF).g(se).c(0x8000).i(W::I32And).i(W::I32Eqz).i(W::Select).i(W::Return);
    x.i(W::End);
    // normal double: integer bit, 11 zero low bits, exponent in range
    x.g(se).c(0x7fff).i(W::I32And).c(16383 - 1023).i(W::I32Sub).s(e);
    x.g(mant).i(W::I64Const(0)).i(W::I64GeS); // integer bit clear
    x.g(mant).i(W::I64Const(0x7ff)).i(W::I64And).i(W::I64Eqz).i(W::I32Eqz).i(W::I32Or);
    x.g(e).c(1).i(W::I32LtS).i(W::I32Or);
    x.g(e).c(2046).i(W::I32GtS).i(W::I32Or).nan_if();
    x.g(se).c(15).i(W::I32ShrU).i(W::I64ExtendI32U).i(W::I64Const(63)).i(W::I64Shl);
    x.g(e).i(W::I64ExtendI32U).i(W::I64Const(52)).i(W::I64Shl).i(W::I64Or);
    x.g(mant).i(W::I64Const(11)).i(W::I64ShrU).i(W::I64Const((1 << 52) - 1)).i(W::I64And).i(W::I64Or);
    x.i(W::F64ReinterpretI64);
    x.finish()
}

/// (address, v): stores the 80-bit image of a zero, infinity or normal double.
fn h_x80_store() -> Function {
    let (addr, v, bits, exp, sign) = (0, 1, 2, 3, 4);
    let mut x = B::new(vec![(1, ValType::I64), (2, ValType::I32)]);
    x.g(v).i(W::I64ReinterpretF64).s(bits);
    x.g(bits).i(W::I64Const(52)).i(W::I64ShrU).i(W::I32WrapI64).c(0x7ff).i(W::I32And).s(exp);
    x.g(bits).i(W::I64Const(63)).i(W::I64ShrU).i(W::I32WrapI64).c(15).i(W::I32Shl).s(sign);
    // significand: 0 for zero, the integer bit alone for infinity, else 1.frac
    // (select(a, b, c) = c ? a : b)
    x.g(addr);
    x.i(W::I64Const(i64::MIN));
    x.i(W::I64Const(i64::MIN)).g(bits).i(W::I64Const(11)).i(W::I64Shl).i(W::I64Or);
    x.g(exp).c(0x7ff).i(W::I32Eq).i(W::Select);
    x.i(W::I64Const(0));
    x.g(exp).i(W::Select);
    x.i(W::I64Store(ma(0)));
    // sign and exponent
    x.g(addr).g(sign);
    x.c(0x7fff);
    x.g(exp).c(16383 - 1023).i(W::I32Add);
    x.g(exp).c(0x7ff).i(W::I32Eq).i(W::Select);
    x.c(0);
    x.g(exp).i(W::Select);
    x.i(W::I32Or).i(W::I32Store16(ma(8)));
    x.finish()
}

/// (dst, q): stores the 80-bit image of the integer q (always exact).
fn h_i64_ext() -> Function {
    let (dst, q, a, lz, neg) = (0, 1, 2, 3, 4);
    let mut x = B::new(vec![(1, ValType::I64), (2, ValType::I32)]);
    x.g(q).i(W::I64Eqz).i(W::If(BlockType::Empty));
    x.g(dst).i(W::I64Const(0)).i(W::I64Store(ma(0))).g(dst).c(0).i(W::I32Store16(ma(8))).i(W::Return);
    x.i(W::End);
    x.g(q).i(W::I64Const(0)).i(W::I64LtS).s(neg);
    // |q| as unsigned (i64::MIN stays 2^63)
    x.i(W::I64Const(0)).g(q).i(W::I64Sub).g(q).g(neg).i(W::Select).s(a);
    x.g(a).i(W::I64Clz).i(W::I32WrapI64).s(lz);
    x.g(dst).g(a).g(lz).i(W::I64ExtendI32U).i(W::I64Shl).i(W::I64Store(ma(0)));
    x.g(dst).g(neg).c(15).i(W::I32Shl).c(16383 + 63).g(lz).i(W::I32Sub).i(W::I32Or).i(W::I32Store16(ma(8)));
    x.finish()
}

/// (src, dst) -> 1 after storing the 80-bit value at src to dst as an i64, when it is
/// an integer in range (no rounding, no exception); 0 otherwise, storing nothing.
fn h_ext_i64() -> Function {
    let (src, dst, mant, v, se, e) = (0, 1, 2, 3, 4, 5);
    let mut x = B::new(vec![(2, ValType::I64), (2, ValType::I32)]);
    x.g(src).i(W::I64Load(ma(0))).s(mant);
    x.g(src).i(W::I32Load16U(ma(8))).s(se);
    x.g(se).c(0x7fff).i(W::I32And).c(16383).i(W::I32Sub).s(e);
    // zero
    x.g(se).c(0x7fff).i(W::I32And).i(W::I32Eqz).g(mant).i(W::I64Eqz).i(W::I32And).i(W::If(BlockType::Empty));
    x.g(dst).i(W::I64Const(0)).i(W::I64Store(ma(0))).c(1).i(W::Return);
    x.i(W::End);
    // integer bit clear (unnormal, denormal), a fraction, or beyond 2^63
    x.g(mant).i(W::I64Const(0)).i(W::I64GeS).g(e).c(0).i(W::I32LtS).i(W::I32Or).g(e).c(63).i(W::I32GtS).i(W::I32Or);
    x.i(W::If(BlockType::Empty)).c(0).i(W::Return).i(W::End);
    // -2^63 is the only value with e = 63
    x.g(e).c(63).i(W::I32Eq).i(W::If(BlockType::Empty));
    x.g(se).c(0x8000).i(W::I32And).g(mant).i(W::I64Const(i64::MIN)).i(W::I64Eq).i(W::I32And).i(W::If(BlockType::Empty));
    x.g(dst).i(W::I64Const(i64::MIN)).i(W::I64Store(ma(0))).c(1).i(W::Return);
    x.i(W::End);
    x.c(0).i(W::Return);
    x.i(W::End);
    // bits below the binary point must be zero
    x.g(mant).i(W::I64Const(1)).c(63).g(e).i(W::I32Sub).i(W::I64ExtendI32U).i(W::I64Shl).i(W::I64Const(1)).i(W::I64Sub).i(W::I64And);
    x.i(W::I64Const(0)).i(W::I64Ne).i(W::If(BlockType::Empty)).c(0).i(W::Return).i(W::End);
    x.g(mant).c(63).g(e).i(W::I32Sub).i(W::I64ExtendI32U).i(W::I64ShrU).s(v);
    x.g(dst);
    x.i(W::I64Const(0)).g(v).i(W::I64Sub).g(v).g(se).c(0x8000).i(W::I32And).i(W::Select);
    x.i(W::I64Store(ma(0)));
    x.c(1);
    x.finish()
}
