//! Random x87 programs: the data block holds plausible operands by region and the
//! initial register stack is partly filled; control words vary precision and rounding.

use crate::casegen::{Case, Gen};
use crate::proto::{CODE_BASE, DATA_BASE};
use iced_x86::{Code, Encoder, Instruction, MemoryOperand, Mnemonic, OpCodeOperandKind, Register};
use rand::RngExt;

// Data block regions (offsets).
const F32_REGION: (u32, u32) = (0x000, 0x200);
const F64_REGION: (u32, u32) = (0x200, 0x400);
const F80_REGION: (u32, u32) = (0x400, 0x500);
const INT_REGION: (u32, u32) = (0x500, 0x600);
const CW_REGION: (u32, u32) = (0x600, 0x680);
const SCRATCH: (u32, u32) = (0x680, 0x740); // + up to 108 bytes stays inside the block

pub fn x87_mnemonics() -> Vec<Mnemonic> {
    use Mnemonic::*;
    vec![
        Fld, Fst, Fstp, Fild, Fist, Fistp, Fisttp, Fxch, Fadd, Faddp, Fiadd, Fsub, Fsubp, Fsubr, Fsubrp, Fisub, Fisubr, Fmul, Fmulp,
        Fimul, Fdiv, Fdivp, Fdivr, Fdivrp, Fidiv, Fidivr, Fcom, Fcomp, Fcompp, Fucom, Fucomp, Fucompp, Ficom, Ficomp, Fcomi,
        Fcomip, Fucomi, Fucomip, Ftst, Fxam, Fchs, Fabs, Fsqrt, Frndint, Fscale, Fxtract, Fprem, Fprem1, Fld1, Fldz, Fldpi,
        Fldl2e, Fldl2t, Fldlg2, Fldln2, Fnstsw, Fnstcw, Fldcw, Fnclex, Fninit, Ffree, Fincstp, Fdecstp, Fnop, Fcmovb, Fcmove,
        Fcmovbe, Fcmovu, Fcmovnb, Fcmovne, Fcmovnbe, Fcmovnu, Fnstenv, Fldenv, Fnsave, Frstor, Fbld, Fbstp,
    ]
}

/// The instructions translated code runs inline (recomp::x87), plus a few that are
/// not, to mix states.
pub fn fast_mnemonics() -> Vec<Mnemonic> {
    use Mnemonic::*;
    vec![
        Fld, Fst, Fstp, Fild, Fist, Fistp, Fisttp, Fxch, Fadd, Faddp, Fiadd, Fsub, Fsubp, Fsubr, Fsubrp, Fisub, Fisubr, Fmul, Fmulp,
        Fimul, Fdiv, Fdivp, Fdivr, Fdivrp, Fidiv, Fidivr, Fcom, Fcomp, Fcompp, Fucom, Fucomp, Fucompp, Ficom, Ficomp, Fcomi,
        Fcomip, Fucomi, Fucomip, Ftst, Fchs, Fabs, Frndint, Fld1, Fldz, Fnstsw, Fnstcw, Fldcw, Ffree, Fincstp, Fdecstp, Wait,
        Fsqrt, Fxam, Fldpi,
    ]
}

pub fn transcendental_mnemonics() -> Vec<Mnemonic> {
    use Mnemonic::*;
    vec![Fsin, Fcos, Fsincos, Fptan, Fpatan, Fyl2x, Fyl2xp1, F2xm1]
}

pub fn x87_codes(mnemonics: &[Mnemonic]) -> Vec<Code> {
    let mut v = Vec::new();
    for code in Code::values() {
        let oc = code.op_code();
        if !oc.is_instruction() || !oc.mode32() || !mnemonics.contains(&code.mnemonic()) {
            continue;
        }
        let name = format!("{code:?}");
        if name.contains("_m94byte") || name.contains("_m14byte") {
            continue; // 16-bit environment images: not used by 32-bit code
        }
        let ok = (0..oc.op_count()).all(|n| matches!(oc.op_kind(n), OpCodeOperandKind::st0 | OpCodeOperandKind::sti_opcode | OpCodeOperandKind::mem | OpCodeOperandKind::ax | OpCodeOperandKind::None));
        if ok {
            v.push(code);
        }
    }
    v
}

fn region_for(mem: iced_x86::MemorySize, g: &mut Gen) -> (u32, u32, u32) {
    use iced_x86::MemorySize as M;
    // (region start, region end, alignment)
    match mem {
        M::Float32 => (F32_REGION.0, F32_REGION.1, 4),
        M::Float64 => (F64_REGION.0, F64_REGION.1, 8),
        M::Float80 => (F80_REGION.0, F80_REGION.1, 2),
        M::Int16 | M::Int32 | M::Int64 | M::Bcd => (INT_REGION.0, INT_REGION.1, 8),
        M::UInt16 => (CW_REGION.0, CW_REGION.1, 2),
        _ => {
            let _ = g;
            (SCRATCH.0, SCRATCH.1, 16)
        }
    }
}

fn random_f32(g: &mut Gen) -> f32 {
    match g.rng.random_range(0..10) {
        0 => 0.0,
        1 => -0.0,
        2 => f32::INFINITY,
        3 => f32::NAN,
        4 => f32::from_bits(g.rng.random::<u32>() & 0x007f_ffff), // denormal
        _ => {
            let e: i32 = g.rng.random_range(-30..30);
            let m: f32 = g.rng.random_range(1.0..2.0);
            let s = if g.rng.random_bool(0.5) { -1.0 } else { 1.0 };
            s * m * 2f32.powi(e)
        }
    }
}

fn random_f64(g: &mut Gen) -> f64 {
    match g.rng.random_range(0..12) {
        0 => 0.0,
        1 => -0.0,
        2 => f64::INFINITY,
        3 => f64::NAN,
        4 => f64::from_bits(g.rng.random::<u64>() & 0x000f_ffff_ffff_ffff),
        5 => g.rng.random_range(-100i64..100) as f64, // small integers: exact conversions
        _ => {
            let e: i32 = g.rng.random_range(-60..60);
            let m: f64 = g.rng.random_range(1.0..2.0);
            let s = if g.rng.random_bool(0.5) { -1.0 } else { 1.0 };
            s * m * 2f64.powi(e)
        }
    }
}

/// Doubles for the inline paths: mostly ordinary values, with zeros, infinities,
/// integers and halves (rounding ties), and values near the limits the inline code
/// checks (single/double overflow and underflow, integer ranges).
fn fast_f64(g: &mut Gen) -> f64 {
    let s = if g.rng.random_bool(0.5) { -1.0 } else { 1.0 };
    match g.rng.random_range(0..40) {
        0 => 0.0,
        1 => -0.0,
        2 => s * f64::INFINITY,
        3 => s * g.rng.random_range(-40000i64..40000) as f64 + if g.rng.random_bool(0.5) { 0.5 } else { 0.0 },
        4 => s * [32767.5, 32768.0, 32768.5, 2147483647.5, 2147483648.0, 2147483648.5, 9223372036854775807.0, 9223372036854774784.0, 4503599627370496.5][g.rng.random_range(0..9)],
        5 => s * g.rng.random_range(1.0..2.0) * 2f64.powi(g.rng.random_range(1015..1024)),
        6 => s * g.rng.random_range(1.0..2.0) * 2f64.powi(g.rng.random_range(-1022..-950)),
        7 => s * g.rng.random_range(1.0..2.0) * 2f64.powi(g.rng.random_range(-130..-120)),
        8 => s * g.rng.random_range(1.0..2.0) * 2f64.powi(g.rng.random_range(125..130)),
        9 => s * (1.0 + g.rng.random_range(0..8) as f64 * f64::EPSILON),
        _ => s * g.rng.random_range(1.0..2.0) * 2f64.powi(g.rng.random_range(-40..40)),
    }
}

/// The 80-bit image of a double (zero, infinity or normal).
fn f80_of(v: f64) -> [u8; 10] {
    let bits = fpu::block::f64_to_ext(v);
    bits.to_le_bytes()[..10].try_into().unwrap()
}

fn random_f80(g: &mut Gen) -> [u8; 10] {
    let mut b = [0u8; 10];
    match g.rng.random_range(0..10) {
        0 => {}
        1 => b[9] = 0x80,
        2 => { b[7] = 0x80; b[8] = 0xff; b[9] = 0x7f; } // +inf
        3 => { b[7] = 0xc0; b[8] = 0xff; b[9] = 0x7f; } // qnan
        _ => {
            let mant: u64 = g.rng.random::<u64>() | (1 << 63);
            let exp: u16 = (16383i32 + g.rng.random_range(-70..70)) as u16 | if g.rng.random_bool(0.5) { 0x8000 } else { 0 };
            b[..8].copy_from_slice(&mant.to_le_bytes());
            b[8..10].copy_from_slice(&exp.to_le_bytes());
        }
    }
    b
}

#[derive(Clone, Copy, PartialEq)]
pub enum Style {
    Plain,
    /// Operands stay inside the domains of the transcendental instructions.
    Trans,
    /// Doubles in registers and memory, 53-bit precision: the inline paths.
    Fast,
}

pub fn x87_case(g: &mut Gen, codes: &[Code], max_instrs: usize) -> Option<Case> {
    x87_case_mode(g, codes, max_instrs, Style::Plain)
}

/// A random control word: exceptions masked (all, or as GameMaker runs: IE/ZE/OE
/// unmasked), random precision and rounding; `fast` mostly picks 53 bits and nearest.
fn random_cw(g: &mut Gen, fast: bool) -> u16 {
    let masks = if fast && g.rng.random_bool(0.3) { 0x32 } else { 0x3f };
    let pc = if fast && g.rng.random_bool(0.85) { 2 } else { g.rng.random_range(0..4u16) };
    let rc = if fast && g.rng.random_bool(0.7) { 0 } else { g.rng.random_range(0..4u16) };
    masks | (pc << 8) | (rc << 10)
}

pub fn x87_case_mode(g: &mut Gen, codes: &[Code], max_instrs: usize, style: Style) -> Option<Case> {
    let trans = style == Style::Trans;
    let fast = style == Style::Fast;
    let n = g.rng.random_range(1..=max_instrs);
    let mut instrs = Vec::new();
    let mut used = Vec::new();
    for _ in 0..n {
        let code = codes[g.rng.random_range(0..codes.len())];
        let oc = code.op_code();
        let mut ops: Vec<Register> = Vec::new();
        let mut mem: Option<MemoryOperand> = None;
        for k in 0..oc.op_count() {
            match oc.op_kind(k) {
                OpCodeOperandKind::st0 => ops.push(Register::ST0),
                OpCodeOperandKind::sti_opcode => ops.push(Register::ST0 + g.rng.random_range(0..8)),
                OpCodeOperandKind::ax => ops.push(Register::AX),
                OpCodeOperandKind::mem => {
                    // Probe the memory size through a dummy instruction.
                    let probe = Instruction::with1(code, MemoryOperand::with_base(Register::EBX)).ok()?;
                    let (lo, hi, align) = region_for(probe.memory_size(), g);
                    let off = g.rng.random_range(lo..hi - 16) & !(align - 1);
                    mem = Some(MemoryOperand::with_base_displ(Register::EBX, off as i64 - 0x200));
                }
                _ => return None,
            }
        }
        let instr = match (oc.op_count(), mem) {
            (0, _) => Instruction::with(code),
            (1, Some(m)) => Instruction::with1(code, m).ok()?,
            (1, None) => Instruction::with1(code, ops[0]).ok()?,
            (2, None) => Instruction::with2(code, ops[0], ops[1]).ok()?,
            _ => return None,
        };
        instrs.push(instr);
        used.push(code);
    }
    let mut enc = Encoder::new(32);
    let mut ip = CODE_BASE as u64;
    for i in instrs.iter_mut() {
        i.set_ip(ip);
        ip += enc.encode(i, ip).ok()? as u64;
    }
    let mut code = enc.take_buffer();
    code.push(0xF4);
    let mut input = g.fresh_input(code);
    input.regs[3] = DATA_BASE + 0x200;
    // Fill operand regions.
    for off in (F32_REGION.0..F32_REGION.1).step_by(4) {
        let v = if fast && g.rng.random_bool(0.7) { fast_f64(g) as f32 } else { random_f32(g) };
        input.data[off as usize..off as usize + 4].copy_from_slice(&v.to_le_bytes());
    }
    for off in (F64_REGION.0..F64_REGION.1).step_by(8) {
        let v = if fast && g.rng.random_bool(0.8) { fast_f64(g) } else { random_f64(g) };
        input.data[off as usize..off as usize + 8].copy_from_slice(&v.to_le_bytes());
    }
    for off in (F80_REGION.0..F80_REGION.1).step_by(16) {
        let v = if fast && g.rng.random_bool(0.6) { f80_of(fast_f64(g)) } else { random_f80(g) };
        input.data[off as usize..off as usize + 10].copy_from_slice(&v);
    }
    for off in (INT_REGION.0..INT_REGION.1).step_by(8) {
        let v: i64 = match g.rng.random_range(0..4) {
            0 => g.rng.random_range(-1000..1000),
            1 => g.rng.random::<i16>() as i64,
            2 => g.rng.random::<i32>() as i64,
            _ => g.rng.random(),
        };
        input.data[off as usize..off as usize + 8].copy_from_slice(&v.to_le_bytes());
    }
    for off in (CW_REGION.0..CW_REGION.1).step_by(2) {
        let cw = random_cw(g, fast);
        input.data[off as usize..off as usize + 2].copy_from_slice(&cw.to_le_bytes());
    }
    // Initial FPU state: random control word, TOP = 0, k registers loaded.
    let cw = random_cw(g, fast);
    input.fx[0..2].copy_from_slice(&cw.to_le_bytes());
    input.fx[2..4].copy_from_slice(&0u16.to_le_bytes());
    let k = g.rng.random_range(0..=8usize);
    let mut abridged = 0u8;
    for i in 0..k {
        let v = if trans {
            // |x| < 0.25 keeps fyl2xp1/f2xm1 in range; positive values keep fyl2x defined
            let f: f64 = g.rng.random_range(0.001..0.25) * if g.rng.random_bool(0.3) { -1.0 } else { 1.0 };
            let mut b = [0u8; 10];
            let bits = f.to_bits();
            let (m, e) = ((bits & ((1 << 52) - 1)) | (1 << 52), ((bits >> 52) & 0x7ff) as i32 - 1023);
            b[..8].copy_from_slice(&(m << 11).to_le_bytes());
            b[8..10].copy_from_slice(&(((e + 16383) as u16) | if f < 0.0 { 0x8000 } else { 0 }).to_le_bytes());
            b
        } else if fast && g.rng.random_bool(0.85) {
            f80_of(fast_f64(g))
        } else {
            random_f80(g)
        };
        input.fx[32 + 16 * i..42 + 16 * i].copy_from_slice(&v);
        abridged |= 1 << i;
    }
    input.fx[4] = abridged;
    Some(Case { input, instrs, codes: used, entries: Vec::new() })
}
