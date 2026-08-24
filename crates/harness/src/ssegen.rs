//! Random SSE/SSE2 programs: xmm registers start with a mix of floats, doubles,
//! integers and special values; memory operands are aligned where the form needs it.

use crate::casegen::{Case, Gen};
use crate::proto::{CODE_BASE, DATA_BASE};
use iced_x86::{Code, Encoder, Instruction, MemoryOperand, Mnemonic, OpCodeOperandKind, Register};
use rand::RngExt;

pub fn sse_mnemonics() -> Vec<Mnemonic> {
    use Mnemonic::*;
    vec![
        Movaps, Movups, Movapd, Movupd, Movdqa, Movdqu, Movsd, Movss, Movq, Movd, Movlps, Movlpd, Movhps, Movhpd, Movhlps,
        Movlhps, Movmskps, Movmskpd, Pmovmskb, Pextrw, Pinsrw, Addsd, Subsd, Mulsd, Divsd, Addss, Subss, Mulss, Divss, Sqrtsd,
        Sqrtss, Minsd, Maxsd, Minss, Maxss, Addps, Subps, Mulps, Divps, Addpd, Subpd, Mulpd, Divpd, Sqrtps, Sqrtpd, Minps,
        Maxps, Minpd, Maxpd, Andps, Andpd, Pand, Orps, Orpd, Por, Xorps, Xorpd, Pxor, Andnps, Andnpd, Pandn, Cvtss2sd,
        Cvtsd2ss, Cvtsi2sd, Cvtsi2ss, Cvtsd2si, Cvttsd2si, Cvtss2si, Cvttss2si, Cvtps2pd, Cvtpd2ps, Cvtdq2pd, Cvtdq2ps,
        Cvttps2dq, Cvtps2dq, Cvttpd2dq, Cvtpd2dq, Comiss, Ucomiss, Comisd, Ucomisd, Cmpps, Cmppd, Cmpss, Cmpsd, Pcmpeqb,
        Pcmpeqw, Pcmpeqd, Pcmpgtb, Pcmpgtw, Pcmpgtd, Paddb, Paddw, Paddd, Paddq, Psubb, Psubw, Psubd, Psubq, Paddsb, Paddsw,
        Paddusb, Paddusw, Psubsb, Psubsw, Psubusb, Psubusw, Pmullw, Pmaddwd, Pmaxsw, Pminsw, Pmaxub, Pminub, Pavgb, Pavgw,
        Pmulhw, Pmulhuw, Pmuludq, Packuswb, Packsswb, Packssdw, Psllw, Pslld, Psllq, Psrlw, Psrld, Psrlq, Psraw, Psrad,
        Pslldq, Psrldq, Pshufd, Pshuflw, Pshufhw, Shufps, Shufpd, Unpcklps, Unpckhps, Unpcklpd, Unpckhpd, Punpcklbw,
        Punpcklwd, Punpckldq, Punpcklqdq, Punpckhbw, Punpckhwd, Punpckhdq, Punpckhqdq, Ldmxcsr,
    ]
}

pub fn sse_codes(mnemonics: &[Mnemonic]) -> Vec<Code> {
    let mut v = Vec::new();
    for code in Code::values() {
        let oc = code.op_code();
        if !oc.is_instruction() || !oc.mode32() || !mnemonics.contains(&code.mnemonic()) {
            continue;
        }
        let name = format!("{code:?}");
        // xmm forms only: no MMX (mm), no AVX (VEX), no 64-bit operands.
        if name.starts_with("VEX") || name.starts_with("EVEX") || name.contains("mm_") && !name.contains("xmm") || name.contains("r64") || name.contains("mmm64") || name.contains("_mm") {
            continue;
        }
        let ok = (0..oc.op_count()).all(|n| {
            matches!(
                oc.op_kind(n),
                OpCodeOperandKind::xmm_reg | OpCodeOperandKind::xmm_rm | OpCodeOperandKind::xmm_or_mem | OpCodeOperandKind::mem
                    | OpCodeOperandKind::r32_reg | OpCodeOperandKind::r32_or_mem | OpCodeOperandKind::r32_rm | OpCodeOperandKind::imm8
            )
        });
        if ok {
            v.push(code);
        }
    }
    v
}

fn random_lane_f32(g: &mut Gen) -> u32 {
    match g.rng.random_range(0..10) {
        0 => 0,
        1 => 0x8000_0000,
        2 => 0x7f80_0000,
        3 => 0x7fc0_0000,
        4 => g.rng.random::<u32>() & 0x007f_ffff,
        5 => (g.rng.random_range(-100i32..100) as f32).to_bits(),
        _ => {
            let e: i32 = g.rng.random_range(-20..40);
            let m: f32 = g.rng.random_range(1.0..2.0);
            (if g.rng.random_bool(0.5) { -m } else { m } * 2f32.powi(e)).to_bits()
        }
    }
}

fn random_lane_f64(g: &mut Gen) -> u64 {
    match g.rng.random_range(0..10) {
        0 => 0,
        1 => 0x8000_0000_0000_0000,
        2 => 0x7ff0_0000_0000_0000,
        3 => 0x7ff8_0000_0000_0000,
        4 => g.rng.random::<u64>() & 0x000f_ffff_ffff_ffff,
        5 => (g.rng.random_range(-100i32..100) as f64).to_bits(),
        _ => {
            let e: i32 = g.rng.random_range(-40..40);
            let m: f64 = g.rng.random_range(1.0..2.0);
            (if g.rng.random_bool(0.5) { -m } else { m } * 2f64.powi(e)).to_bits()
        }
    }
}

fn random_xmm(g: &mut Gen) -> [u8; 16] {
    let mut b = [0u8; 16];
    match g.rng.random_range(0..4) {
        0 => {
            for l in 0..4 {
                b[4 * l..4 * l + 4].copy_from_slice(&random_lane_f32(g).to_le_bytes());
            }
        }
        1 => {
            for l in 0..2 {
                b[8 * l..8 * l + 8].copy_from_slice(&random_lane_f64(g).to_le_bytes());
            }
        }
        2 => {
            for l in 0..4 {
                let v: i32 = if g.rng.random_bool(0.5) { g.rng.random_range(-1000..1000) } else { g.rng.random() };
                b[4 * l..4 * l + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => g.rng.fill(&mut b[..]),
    }
    b
}

pub fn sse_case(g: &mut Gen, codes: &[Code], max_instrs: usize) -> Option<Case> {
    let n = g.rng.random_range(1..=max_instrs);
    let mut instrs = Vec::new();
    let mut used = Vec::new();
    for _ in 0..n {
        let code = codes[g.rng.random_range(0..codes.len())];
        let oc = code.op_code();
        let name = format!("{code:?}");
        let aligned = name.contains("Movaps") || name.contains("Movapd") || name.contains("Movdqa") || name.contains("Movnt")
            || oc.op_count() > 1 && (0..oc.op_count()).any(|k| oc.op_kind(k) == OpCodeOperandKind::xmm_or_mem)
                && !(name.contains("Movups") || name.contains("Movupd") || name.contains("Movdqu") || name.contains("Lddqu"))
                && name.contains("xmmm128");
        let mut ops: Vec<(u8, u64)> = Vec::new(); // (kind, value): 0 xmm, 1 r32, 2 mem, 3 imm
        for k in 0..oc.op_count() {
            match oc.op_kind(k) {
                OpCodeOperandKind::xmm_reg | OpCodeOperandKind::xmm_rm => ops.push((0, g.rng.random_range(0..8))),
                OpCodeOperandKind::xmm_or_mem => {
                    if g.rng.random_bool(0.6) { ops.push((0, g.rng.random_range(0..8))) } else { ops.push((2, 0)) }
                }
                OpCodeOperandKind::mem => ops.push((2, 0)),
                OpCodeOperandKind::r32_reg | OpCodeOperandKind::r32_rm => ops.push((1, [0u64, 1, 2][g.rng.random_range(0..3)])),
                OpCodeOperandKind::r32_or_mem => {
                    if g.rng.random_bool(0.5) { ops.push((1, [0u64, 1, 2][g.rng.random_range(0..3)])) } else { ops.push((2, 0)) }
                }
                OpCodeOperandKind::imm8 => ops.push((3, g.rng.random_range(0..256))),
                _ => return None,
            }
        }
        let mem = {
            // memory operand: inside the data block, aligned to 16 when required;
            // ldmxcsr must read a valid control word (reserved bits set fault natively)
            let off = g.rng.random_range(0x40u32..0x780);
            let off = if code.mnemonic() == Mnemonic::Ldmxcsr { off & !63 } else if aligned { off & !15 } else { off & !3 };
            MemoryOperand::with_base_displ(Register::EBX, off as i64 - 0x200)
        };
        let to_reg = |k: u8, v: u64| -> Register {
            match k {
                0 => Register::XMM0 + v as u32,
                _ => Register::EAX + v as u32,
            }
        };
        let instr = match ops.as_slice() {
            [(2, _)] => Instruction::with1(code, mem).ok()?,
            [(k, v)] => Instruction::with1(code, to_reg(*k, *v)).ok()?,
            [(k0, v0), (3, i)] => Instruction::with2(code, to_reg(*k0, *v0), *i as i32).ok()?,
            [(k0, v0), (2, _)] => Instruction::with2(code, to_reg(*k0, *v0), mem).ok()?,
            [(2, _), (k1, v1)] => Instruction::with2(code, mem, to_reg(*k1, *v1)).ok()?,
            [(k0, v0), (k1, v1)] => Instruction::with2(code, to_reg(*k0, *v0), to_reg(*k1, *v1)).ok()?,
            [(k0, v0), (2, _), (3, i)] => Instruction::with3(code, to_reg(*k0, *v0), mem, *i as i32).ok()?,
            [(k0, v0), (k1, v1), (3, i)] => Instruction::with3(code, to_reg(*k0, *v0), to_reg(*k1, *v1), *i as i32).ok()?,
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
    // Small values in the integer registers used by cvtsi2*/pinsrw/movd.
    for r in 0..3 {
        if g.rng.random_bool(0.5) {
            input.regs[r] = g.rng.random_range(-100000i32..100000) as u32;
        }
    }
    // Data block: lanes of plausible floats/doubles/ints.
    for off in (0..0x800usize).step_by(16) {
        input.data[off..off + 16].copy_from_slice(&random_xmm(g));
    }
    // A valid MXCSR for ldmxcsr somewhere in the block: default, masked.
    for off in (0x40..0x780usize).step_by(64) {
        input.data[off..off + 4].copy_from_slice(&0x1f80u32.to_le_bytes());
    }
    for x in 0..8 {
        input.fx[160 + 16 * x..176 + 16 * x].copy_from_slice(&random_xmm(g));
    }
    Some(Case { input, instrs, codes: used, entries: Vec::new() })
}
