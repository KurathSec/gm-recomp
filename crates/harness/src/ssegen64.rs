//! Random SSE/SSE2 programs in 64-bit mode: all sixteen xmm registers, and the 64-bit
//! integer forms (movq/cvtsi2s*/cvt(t)s*2si with a 64-bit GPR). Mirrors `ssegen`.

use crate::casegen64::{Case64, Gen64};
use crate::proto::{CODE_BASE, DATA_BASE};
use iced_x86::{Code, Encoder, Instruction, MemoryOperand, Mnemonic, OpCodeOperandKind, Register};
use rand::RngExt;

/// The xmm SSE/SSE2 forms the 64-bit lifter handles (the same lifter as 32-bit, plus
/// movq/cvtsi2s*/cvt(t)s*2si with a 64-bit integer).
pub fn sse_codes(mnemonics: &[Mnemonic]) -> Vec<Code> {
    let mut v = Vec::new();
    for code in Code::values() {
        let oc = code.op_code();
        if !oc.is_instruction() || !oc.mode64() || !mnemonics.contains(&code.mnemonic()) {
            continue;
        }
        let name = format!("{code:?}");
        if name.starts_with("VEX") || name.starts_with("EVEX") || name.contains("mm_") && !name.contains("xmm") || name.contains("_mm") {
            continue;
        }
        let ok = (0..oc.op_count()).all(|n| {
            matches!(
                oc.op_kind(n),
                OpCodeOperandKind::xmm_reg
                    | OpCodeOperandKind::xmm_rm
                    | OpCodeOperandKind::xmm_or_mem
                    | OpCodeOperandKind::mem
                    | OpCodeOperandKind::r32_reg
                    | OpCodeOperandKind::r32_or_mem
                    | OpCodeOperandKind::r32_rm
                    | OpCodeOperandKind::r64_reg
                    | OpCodeOperandKind::r64_or_mem
                    | OpCodeOperandKind::r64_rm
                    | OpCodeOperandKind::imm8
            )
        });
        if ok {
            v.push(code);
        }
    }
    v
}

fn random_lane_f64(g: &mut Gen64) -> u64 {
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

fn random_xmm(g: &mut Gen64) -> [u8; 16] {
    let mut b = [0u8; 16];
    match g.rng.random_range(0..3) {
        0 => {
            for l in 0..2 {
                b[8 * l..8 * l + 8].copy_from_slice(&random_lane_f64(g).to_le_bytes());
            }
        }
        1 => {
            for l in 0..4 {
                let v: i32 = if g.rng.random_bool(0.5) { g.rng.random_range(-1000..1000) } else { g.rng.random() };
                b[4 * l..4 * l + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => g.rng.fill(&mut b[..]),
    }
    b
}

pub fn sse_case(g: &mut Gen64, codes: &[Code], max_instrs: usize) -> Option<Case64> {
    let n = g.rng.random_range(1..=max_instrs);
    let mut instrs = Vec::new();
    let mut used = Vec::new();
    for _ in 0..n {
        let code = codes[g.rng.random_range(0..codes.len())];
        let oc = code.op_code();
        let name = format!("{code:?}");
        let aligned = name.contains("Movaps") || name.contains("Movapd") || name.contains("Movdqa") || name.contains("Movnt")
            || oc.op_count() > 1
                && (0..oc.op_count()).any(|k| oc.op_kind(k) == OpCodeOperandKind::xmm_or_mem)
                && !(name.contains("Movups") || name.contains("Movupd") || name.contains("Movdqu") || name.contains("Lddqu"))
                && name.contains("xmmm128");
        let mut ops: Vec<(u8, u64)> = Vec::new(); // 0 xmm, 1 r32, 2 mem, 3 imm, 5 r64
        for k in 0..oc.op_count() {
            match oc.op_kind(k) {
                OpCodeOperandKind::xmm_reg | OpCodeOperandKind::xmm_rm => ops.push((0, g.rng.random_range(0..16))),
                OpCodeOperandKind::xmm_or_mem => {
                    if g.rng.random_bool(0.6) {
                        ops.push((0, g.rng.random_range(0..16)))
                    } else {
                        ops.push((2, 0))
                    }
                }
                OpCodeOperandKind::mem => ops.push((2, 0)),
                OpCodeOperandKind::r32_reg | OpCodeOperandKind::r32_rm => ops.push((1, g.rng.random_range(0..3))),
                OpCodeOperandKind::r32_or_mem => {
                    if g.rng.random_bool(0.5) {
                        ops.push((1, g.rng.random_range(0..3)))
                    } else {
                        ops.push((2, 0))
                    }
                }
                OpCodeOperandKind::r64_reg | OpCodeOperandKind::r64_rm => ops.push((5, g.rng.random_range(0..3))),
                OpCodeOperandKind::r64_or_mem => {
                    if g.rng.random_bool(0.5) {
                        ops.push((5, g.rng.random_range(0..3)))
                    } else {
                        ops.push((2, 0))
                    }
                }
                OpCodeOperandKind::imm8 => ops.push((3, g.rng.random_range(0..256))),
                _ => return None,
            }
        }
        // a memory operand in the data block (base RBX), aligned to 16 where required;
        // ldmxcsr must read a valid control word (reserved bits set fault natively)
        let off = g.rng.random_range(0x40u32..0x780);
        let off = if code.mnemonic() == Mnemonic::Ldmxcsr {
            off & !63
        } else if aligned {
            off & !15
        } else {
            off & !3
        };
        let mem = MemoryOperand::with_base_displ(Register::RBX, off as i64 - 0x200);
        let to_reg = |k: u8, v: u64| -> Register {
            match k {
                0 => Register::XMM0 + v as u32,
                5 => Register::RAX + v as u32,
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
    let mut enc = Encoder::new(64);
    let mut ip = CODE_BASE as u64;
    for i in instrs.iter_mut() {
        i.set_ip(ip);
        ip += enc.encode(i, ip).ok()? as u64;
    }
    let mut code = enc.take_buffer();
    code.push(0xF4);
    let mut input = g.fresh_input(code, &[]);
    input.regs[Register::RBX as usize - Register::RAX as usize] = (DATA_BASE + 0x200) as u64;
    for r in 0..3 {
        if g.rng.random_bool(0.5) {
            input.regs[r] = g.rng.random_range(-100000i64..100000) as u64;
        }
    }
    for off in (0..0x800usize).step_by(16) {
        input.data[off..off + 16].copy_from_slice(&random_xmm(g));
    }
    // a valid (default, masked) MXCSR for ldmxcsr every 64 bytes
    for off in (0x40..0x780usize).step_by(64) {
        input.data[off..off + 4].copy_from_slice(&0x1f80u32.to_le_bytes());
    }
    for x in 0..16 {
        input.fx[160 + 16 * x..176 + 16 * x].copy_from_slice(&random_xmm(g));
    }
    Some(Case64 { input, instrs, codes: used, entries: Vec::new() })
}
