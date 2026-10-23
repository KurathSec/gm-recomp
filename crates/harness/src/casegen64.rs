//! Random x86-64 test cases for the differential tester: integer sequences, flow
//! (branches, calls, string ops) and SSE, in 64-bit mode. Mirrors `casegen`/`flowgen`/
//! `ssegen`, but with sixteen 64-bit registers, REX byte registers, RIP-relative memory
//! and the 64-bit operand forms.

use crate::proto::{CODE_BASE, CaseIn64, DATA_SIZE, STACK_SIZE};
use iced_x86::code_asm::{CodeAssembler, CodeLabel};
use iced_x86::{BlockEncoderOptions, Code, Decoder, DecoderOptions, Encoder, Instruction, MemoryOperand, Mnemonic, OpCodeOperandKind, Register};
use rand::RngExt;
use rand::rngs::StdRng;
use rand::SeedableRng;

pub const DATA_BASE: u32 = crate::proto::DATA_BASE;

/// A generated 64-bit case.
pub struct Case64 {
    pub input: Box<CaseIn64>,
    pub instrs: Vec<Instruction>,
    pub codes: Vec<Code>,
    /// Extra function entry addresses (call targets) inside the code block.
    pub entries: Vec<u32>,
}

/// The integer mnemonics the 64-bit lifter claims to support.
pub fn integer_mnemonics() -> Vec<Mnemonic> {
    use Mnemonic::*;
    vec![
        Mov, Movzx, Movsx, Movsxd, Lea, Push, Pop, Add, Adc, Sub, Sbb, Cmp, And, Or, Xor, Test, Inc, Dec, Neg, Not, Shl,
        Sar, Shr, Rol, Ror, Rcl, Rcr, Shld, Shrd, Imul, Mul, Div, Idiv, Cbw, Cwde, Cdqe, Cwd, Cdq, Cqo, Xchg, Cmovo,
        Cmovno, Cmovb, Cmovae, Cmove, Cmovne, Cmovbe, Cmova, Cmovs, Cmovns, Cmovp, Cmovnp, Cmovl, Cmovge, Cmovle, Cmovg,
        Seto, Setno, Setb, Setae, Sete, Setne, Setbe, Seta, Sets, Setns, Setp, Setnp, Setl, Setge, Setle, Setg, Bt, Bts,
        Btr, Btc, Bsf, Bsr, Tzcnt, Lzcnt, Popcnt, Bswap, Xadd, Cmpxchg, Lahf, Sahf, Clc, Stc, Cmc, Cld, Std, Nop,
    ]
}

fn kind_ok(k: OpCodeOperandKind) -> bool {
    use OpCodeOperandKind::*;
    matches!(
        k,
        r8_reg | r8_or_mem | r8_opcode | r16_reg | r16_reg_mem | r16_or_mem | r16_rm | r16_opcode | r32_reg | r32_reg_mem
            | r32_or_mem | r32_rm | r32_opcode | r64_reg | r64_reg_mem | r64_or_mem | r64_rm | r64_opcode | imm8 | imm8sex16
            | imm8sex32 | imm8sex64 | imm16 | imm32 | imm32sex64 | imm64 | mem | al | ax | eax | rax | cl | dx | imm8_const_1
            | OpCodeOperandKind::None
    )
}

/// 64-bit codes whose operands the generator can fill.
pub fn supported_codes(mnemonics: &[Mnemonic]) -> Vec<Code> {
    let mut v = Vec::new();
    for code in Code::values() {
        let oc = code.op_code();
        if !oc.is_instruction() || !oc.mode64() || !mnemonics.contains(&code.mnemonic()) {
            continue;
        }
        let name = format!("{code:?}");
        if name.contains("Sreg") || name.starts_with("VEX") || name.starts_with("EVEX") || name == "Bswap_r16" {
            continue;
        }
        if oc.address_size() == 16 || oc.operand_size() == 16 && matches!(code.mnemonic(), Mnemonic::Shld | Mnemonic::Shrd) {
            // (16-bit shld/shrd counts above 16 are undefined; skip the whole form)
            continue;
        }
        if (0..oc.op_count()).all(|n| kind_ok(oc.op_kind(n))) && oc.op_count() <= 3 {
            v.push(code);
        }
    }
    v
}

/// Registers whose initial value points into the data block.
const BASES: [Register; 6] = [Register::RBX, Register::RSI, Register::RDI, Register::RBP, Register::R14, Register::R15];
const INDEXES: [Register; 4] = [Register::RCX, Register::RDX, Register::R10, Register::R11];

pub struct Gen64 {
    pub rng: StdRng,
}

#[derive(Clone, Copy)]
enum Op {
    R(Register),
    M(MemoryOperand),
    I(i64),
    None,
}

impl Gen64 {
    pub fn new(seed: u64) -> Self {
        Gen64 { rng: StdRng::seed_from_u64(seed) }
    }

    /// A register of `size` bytes from index `n` (0..16). An 8-bit register is a REX one
    /// (spl, bpl, sil, dil, r8b..r15b) for n >= 4, and ah/ch/dh/bh or al/cl/dl/bl below.
    fn reg_at(&mut self, size: u8, n: u32) -> Register {
        match size {
            8 => Register::RAX + n,
            4 => Register::EAX + n,
            2 => Register::AX + n,
            _ if n >= 4 => Register::SPL + (n - 4),
            _ if self.rng.random_bool(0.3) => Register::AH + n,
            _ => Register::AL + n,
        }
    }

    /// Destination / source register of `size`: not the base or index registers in use,
    /// not RSP (keeps the stack valid).
    fn reg(&mut self, size: u8, dest: bool, written: &[Register]) -> Register {
        let mut pool: Vec<u32> = Vec::new();
        for n in 0..16u32 {
            let r64 = Register::RAX + n;
            if r64 == Register::RSP {
                continue;
            }
            if BASES.contains(&r64) && written.contains(&r64) {
                continue;
            }
            if dest && BASES.contains(&r64) {
                continue;
            }
            pool.push(n);
        }
        let n = pool[self.rng.random_range(0..pool.len())];
        self.reg_at(size, n)
    }

    fn mem(&mut self, written: &[Register], rip: bool) -> (MemoryOperand, Option<Register>) {
        if rip && self.rng.random_bool(0.25) {
            // RIP-relative into the data block (iced computes the rel32 at encode time)
            let target = DATA_BASE + self.rng.random_range(0x40..0x7c0);
            return (MemoryOperand::with_base_displ(Register::RIP, target as i64), None);
        }
        let bases: Vec<Register> = BASES.iter().copied().filter(|b| !written.contains(b)).collect();
        if bases.is_empty() || self.rng.random_bool(0.1) {
            let disp = DATA_BASE + self.rng.random_range(0x40..0x7c0);
            return (MemoryOperand::with_displ(disp as u64, 8), None);
        }
        let base = bases[self.rng.random_range(0..bases.len())];
        let disp = self.rng.random_range(-0x80i64..0x80);
        let mut index = None;
        let mut mo = MemoryOperand::with_base_displ(base, disp);
        if self.rng.random_bool(0.4) {
            let ix = INDEXES[self.rng.random_range(0..INDEXES.len())];
            if !written.contains(&ix) {
                let scale = [1u32, 2, 4, 8][self.rng.random_range(0..4)];
                mo = MemoryOperand::with_base_index_scale_displ_size(base, ix, scale, disp, 1);
                index = Some(ix);
            }
        }
        (mo, index)
    }

    fn build_instr(&mut self, code: Code, written: &mut Vec<Register>, index_regs: &mut Vec<Register>) -> Option<Instruction> {
        use OpCodeOperandKind::*;
        let oc = code.op_code();
        let mut ops: Vec<Op> = Vec::new();
        for n in 0..oc.op_count() {
            let dest = n == 0 && oc.op_count() > 1
                || matches!(code.mnemonic(), Mnemonic::Inc | Mnemonic::Dec | Mnemonic::Neg | Mnemonic::Not | Mnemonic::Pop | Mnemonic::Bswap | Mnemonic::Seto);
            let op = match oc.op_kind(n) {
                r8_reg | r8_opcode => Op::R(self.reg(1, dest, written)),
                r16_reg | r16_reg_mem | r16_rm | r16_opcode => Op::R(self.reg(2, dest, written)),
                r32_reg | r32_reg_mem | r32_rm | r32_opcode => Op::R(self.reg(4, dest, written)),
                r64_reg | r64_reg_mem | r64_rm | r64_opcode => Op::R(self.reg(8, dest, written)),
                r8_or_mem | r16_or_mem | r32_or_mem | r64_or_mem => {
                    if self.rng.random_bool(0.5) {
                        let size = match oc.op_kind(n) {
                            r8_or_mem => 1,
                            r16_or_mem => 2,
                            r32_or_mem => 4,
                            _ => 8,
                        };
                        Op::R(self.reg(size, dest, written))
                    } else {
                        let (m, ix) = self.mem(written, true);
                        if let Some(ix) = ix {
                            index_regs.push(ix);
                        }
                        Op::M(m)
                    }
                }
                mem => {
                    let (m, ix) = self.mem(written, true);
                    if let Some(ix) = ix {
                        index_regs.push(ix);
                    }
                    Op::M(m)
                }
                imm8 => Op::I(self.rng.random_range(0..256)),
                imm8sex16 | imm8sex32 | imm8sex64 => Op::I(self.rng.random_range(-128..128)),
                imm16 => Op::I(self.rng.random_range(0..65536)),
                imm32 => Op::I(self.rng.random::<u32>() as i64),
                imm32sex64 => Op::I(self.rng.random::<i32>() as i64),
                imm64 => Op::I(self.rng.random::<i64>()),
                al => Op::R(Register::AL),
                ax => Op::R(Register::AX),
                eax => Op::R(Register::EAX),
                rax => Op::R(Register::RAX),
                cl => Op::R(Register::CL),
                dx => Op::R(Register::DX),
                imm8_const_1 => Op::I(1),
                OpCodeOperandKind::None => Op::None,
                _ => return Option::None,
            };
            ops.push(op);
        }
        // bt/bts/btr/btc [mem], reg with a wild offset addresses far memory: keep the
        // offset register small.
        if matches!(code.mnemonic(), Mnemonic::Bt | Mnemonic::Bts | Mnemonic::Btr | Mnemonic::Btc) && matches!(ops.first(), Some(Op::M(_))) {
            if let Some(Op::R(r)) = ops.get(1).copied() {
                let small = INDEXES.iter().copied().find(|ix| !written.contains(ix) && !index_regs.contains(ix));
                let Some(ix) = small else { return Option::None };
                ops[1] = Op::R(sized(ix, r.size() as u8));
                index_regs.push(ix);
            }
        }
        // Record destination writes.
        if let Some(Op::R(r)) = ops.first() {
            let src_only = oc.op_count() == 1 && matches!(code.mnemonic(), Mnemonic::Push | Mnemonic::Mul | Mnemonic::Imul | Mnemonic::Div | Mnemonic::Idiv);
            if !src_only {
                written.push(full(*r));
            }
        }
        match code.mnemonic() {
            Mnemonic::Mul | Mnemonic::Imul | Mnemonic::Div | Mnemonic::Idiv | Mnemonic::Cwd | Mnemonic::Cdq | Mnemonic::Cqo
            | Mnemonic::Cbw | Mnemonic::Cwde | Mnemonic::Cdqe => {
                written.push(Register::RAX);
                written.push(Register::RDX);
            }
            Mnemonic::Xchg | Mnemonic::Xadd => {
                if let Some(Op::R(r)) = ops.get(1) {
                    written.push(full(*r));
                }
            }
            Mnemonic::Cmpxchg => written.push(Register::RAX),
            Mnemonic::Pop => {
                if let Some(Op::R(r)) = ops.first() {
                    written.push(full(*r));
                }
            }
            Mnemonic::Lahf => written.push(Register::RAX),
            _ => {}
        }
        build(code, &ops)
    }

    pub fn case(&mut self, codes: &[Code], max_instrs: usize) -> Option<Case64> {
        let n = self.rng.random_range(1..=max_instrs);
        let mut instrs = Vec::new();
        let mut used = Vec::new();
        let mut written = Vec::new();
        let mut index_regs = Vec::new();
        for _ in 0..n {
            let code = codes[self.rng.random_range(0..codes.len())];
            if let Some(i) = self.build_instr(code, &mut written, &mut index_regs) {
                instrs.push(i);
                used.push(code);
            }
        }
        if instrs.is_empty() {
            return None;
        }
        let input = self.encode(&mut instrs, &index_regs)?;
        Some(Case64 { input, instrs, codes: used, entries: Vec::new() })
    }

    /// One filler instruction for a larger program; `written` tracks changed registers.
    pub fn filler(&mut self, codes: &[Code], written: &mut Vec<Register>, allow_stack: bool) -> Option<(Instruction, Code)> {
        let mut index_regs = Vec::new();
        for _ in 0..8 {
            let code = codes[self.rng.random_range(0..codes.len())];
            if !allow_stack && matches!(code.mnemonic(), Mnemonic::Push | Mnemonic::Pop) {
                continue;
            }
            if let Some(i) = self.build_instr(code, written, &mut index_regs) {
                if index_regs.is_empty() {
                    return Some((i, code));
                }
                index_regs.clear();
            }
        }
        None
    }

    /// Encodes the instructions at `CODE_BASE`, appends `hlt`, and makes the initial
    /// state (base registers into the data block, random data/stack, small indexes).
    fn encode(&mut self, instrs: &mut [Instruction], index_regs: &[Register]) -> Option<Box<CaseIn64>> {
        let mut enc = Encoder::new(64);
        let mut ip = CODE_BASE as u64;
        for i in instrs.iter_mut() {
            i.set_ip(ip);
            ip += enc.encode(i, ip).ok()? as u64;
        }
        let mut code = enc.take_buffer();
        code.push(0xF4);
        Some(self.fresh_input(code, index_regs))
    }

    pub fn fresh_input(&mut self, code: Vec<u8>, index_regs: &[Register]) -> Box<CaseIn64> {
        let mut input = CaseIn64::new(code);
        for r in 0..16 {
            input.regs[r] = self.rng.random::<u64>();
        }
        // base registers point inside the data block, well clear of the ends
        input.regs[Register::RBX as usize - Register::RAX as usize] = (DATA_BASE + 0x200) as u64;
        input.regs[Register::RSI as usize - Register::RAX as usize] = (DATA_BASE + 0x300) as u64;
        input.regs[Register::RDI as usize - Register::RAX as usize] = (DATA_BASE + 0x400) as u64;
        input.regs[Register::RBP as usize - Register::RAX as usize] = (DATA_BASE + 0x500) as u64;
        input.regs[Register::R14 as usize - Register::RAX as usize] = (DATA_BASE + 0x600) as u64;
        input.regs[Register::R15 as usize - Register::RAX as usize] = (DATA_BASE + 0x700) as u64;
        input.regs[4] = (0x200 + self.rng.random_range(0..0x20) * 8) as u64;
        for ix in index_regs {
            input.regs[*ix as usize - Register::RAX as usize] = self.rng.random_range(0..16);
        }
        if self.rng.random_bool(0.3) {
            input.regs[0] = self.rng.random_range(0..8) as u64 * 0x5555_5555_5555_5555;
        }
        input.rflags = self.rng.random::<u64>() & 0x8d5;
        self.rng.fill(&mut input.data[..]);
        self.rng.fill(&mut input.stack[..]);
        input
    }
}

/// The same register as `r` at `size` bytes.
fn sized(r: Register, size: u8) -> Register {
    let n = full(r) as u32 - Register::RAX as u32;
    match size {
        8 => Register::RAX + n,
        4 => Register::EAX + n,
        2 => Register::AX + n,
        _ if n < 4 => Register::AL + n,
        _ => Register::SPL + (n - 4),
    }
}

/// The 64-bit register `r` is a part of.
fn full(r: Register) -> Register {
    r.full_register()
}

fn build(code: Code, ops: &[Op]) -> Option<Instruction> {
    match ops.len() {
        0 => Some(Instruction::with(code)),
        1 => with1(code, ops[0]),
        2 => with2(code, ops[0], ops[1]),
        3 => with3(code, ops[0], ops[1], ops[2]),
        _ => None,
    }
}

fn with1(code: Code, a: Op) -> Option<Instruction> {
    match a {
        Op::R(r) => Instruction::with1(code, r).ok(),
        Op::M(m) => Instruction::with1(code, m).ok(),
        Op::I(i) => Instruction::with1(code, i as i32).ok(),
        Op::None => Instruction::with(code).into(),
    }
}

fn with2(code: Code, a: Op, b: Op) -> Option<Instruction> {
    match (a, b) {
        (Op::R(x), Op::R(y)) => Instruction::with2(code, x, y).ok(),
        (Op::R(x), Op::M(y)) => Instruction::with2(code, x, y).ok(),
        (Op::R(x), Op::I(y)) if code.op_code().op_kind(1) == OpCodeOperandKind::imm64 => Instruction::with2(code, x, y).ok(),
        (Op::R(x), Op::I(y)) => Instruction::with2(code, x, y as i32).ok(),
        (Op::M(x), Op::R(y)) => Instruction::with2(code, x, y).ok(),
        (Op::M(x), Op::I(y)) => Instruction::with2(code, x, y as i32).ok(),
        _ => None,
    }
}

fn with3(code: Code, a: Op, b: Op, c: Op) -> Option<Instruction> {
    match (a, b, c) {
        (Op::R(x), Op::R(y), Op::I(z)) => Instruction::with3(code, x, y, z as i32).ok(),
        (Op::R(x), Op::M(y), Op::I(z)) => Instruction::with3(code, x, y, z as i32).ok(),
        (Op::R(x), Op::R(y), Op::R(z)) => Instruction::with3(code, x, y, z).ok(),
        (Op::M(x), Op::R(y), Op::I(z)) => Instruction::with3(code, x, y, z as i32).ok(),
        (Op::M(x), Op::R(y), Op::R(z)) => Instruction::with3(code, x, y, z).ok(),
        _ => None,
    }
}

fn jcc(a: &mut CodeAssembler, k: u32, l: CodeLabel) -> Result<(), iced_x86::IcedError> {
    match k % 16 {
        0 => a.jo(l),
        1 => a.jno(l),
        2 => a.jb(l),
        3 => a.jae(l),
        4 => a.je(l),
        5 => a.jne(l),
        6 => a.jbe(l),
        7 => a.ja(l),
        8 => a.js(l),
        9 => a.jns(l),
        10 => a.jp(l),
        11 => a.jnp(l),
        12 => a.jl(l),
        13 => a.jge(l),
        14 => a.jle(l),
        _ => a.jg(l),
    }
}

/// A small 64-bit program with real control flow (branches, loops, calls, stack
/// traffic, string ops).
pub fn flow_case(g: &mut Gen64, codes: &[Code]) -> Option<Case64> {
    use iced_x86::code_asm::registers::gpr32::*;
    use iced_x86::code_asm::registers::gpr64::*;
    let mut a = CodeAssembler::new(64).ok()?;
    let mut written: Vec<Register> = Vec::new();
    let mut used = Vec::new();
    let mut entries = Vec::new();
    let mut call_targets: Vec<CodeLabel> = Vec::new();
    let fill = |a: &mut CodeAssembler, g: &mut Gen64, written: &mut Vec<Register>, used: &mut Vec<Code>, stack: bool| -> Option<()> {
        let (i, c) = g.filler(codes, written, stack)?;
        a.add_instruction(i).ok()?;
        used.push(c);
        Some(())
    };
    match g.rng.random_range(0..7) {
        6 => {
            // cmpxchg8b / cmpxchg16b [rsi]: the memory is set to match edx:eax (rdx:rax)
            // half the time, so both the equal (ZF=1) and not-equal paths run.
            let wide = g.rng.random_bool(0.5);
            let equal = g.rng.random_bool(0.5);
            let mo = MemoryOperand::with_base(Register::RSI);
            let i = if wide {
                Instruction::with1(Code::Cmpxchg16b_m128, mo).ok()?
            } else {
                Instruction::with1(Code::Cmpxchg8b_m64, mo).ok()?
            };
            a.add_instruction(i).ok()?;
            used.push(if wide { Code::Cmpxchg16b_m128 } else { Code::Cmpxchg8b_m64 });
            written.push(Register::RAX);
            written.push(Register::RDX);
            a.hlt().ok()?;
            let res = a.assemble_options(CODE_BASE as u64, BlockEncoderOptions::NONE).ok()?;
            let code = res.inner.code_buffer.clone();
            let mut input = g.fresh_input(code.clone(), &[]);
            input.regs[Register::RSI as usize - Register::RAX as usize] = (DATA_BASE + 0x300) as u64; // 16-aligned
            let half = if wide { 8 } else { 4 };
            if equal {
                let (lo, hi) = (&input.regs[0].to_le_bytes(), &input.regs[2].to_le_bytes());
                input.data[0x300..0x300 + half].copy_from_slice(&lo[..half]);
                input.data[0x300 + half..0x300 + 2 * half].copy_from_slice(&hi[..half]);
            }
            let mut dec = Decoder::with_ip(64, &code, CODE_BASE as u64, DecoderOptions::NONE);
            let instrs: Vec<Instruction> = dec.iter().collect();
            return Some(Case64 { input, instrs, codes: used, entries });
        }
        0 => {
            let mut l1 = a.create_label();
            let mut l2 = a.create_label();
            fill(&mut a, g, &mut written, &mut used, true)?;
            jcc(&mut a, g.rng.random(), l1).ok()?;
            fill(&mut a, g, &mut written, &mut used, true)?;
            a.jmp(l2).ok()?;
            a.set_label(&mut l1).ok()?;
            fill(&mut a, g, &mut written, &mut used, true)?;
            a.set_label(&mut l2).ok()?;
            fill(&mut a, g, &mut written, &mut used, true)?;
        }
        1 => {
            let mut top = a.create_label();
            a.mov(ecx, g.rng.random_range(1u32..6)).ok()?;
            written.push(Register::RCX);
            a.set_label(&mut top).ok()?;
            a.add(rax, rcx).ok()?;
            a.xor(rdx, rax).ok()?;
            written.push(Register::RAX);
            written.push(Register::RDX);
            a.dec(rcx).ok()?;
            a.jne(top).ok()?;
            fill(&mut a, g, &mut written, &mut used, true)?;
        }
        2 => {
            let mut top = a.create_label();
            let mut out = a.create_label();
            a.mov(rcx, g.rng.random_range(0u64..5)).ok()?;
            written.push(Register::RCX);
            a.jrcxz(out).ok()?;
            a.set_label(&mut top).ok()?;
            a.add(rax, 3i32).ok()?;
            written.push(Register::RAX);
            match g.rng.random_range(0..3) {
                0 => a.loop_(top).ok()?,
                1 => a.loope(top).ok()?,
                _ => a.loopne(top).ok()?,
            }
            a.set_label(&mut out).ok()?;
            fill(&mut a, g, &mut written, &mut used, true)?;
        }
        3 => {
            // call F; A; hlt; F: (prologue) B; ret
            let mut f = a.create_label();
            a.call(f).ok()?;
            fill(&mut a, g, &mut written, &mut used, true)?;
            a.hlt().ok()?;
            a.set_label(&mut f).ok()?;
            call_targets.push(f);
            if g.rng.random_bool(0.4) {
                a.push(rbp).ok()?;
                a.mov(rbp, rsp).ok()?;
                a.sub(rsp, 0x20i32).ok()?;
                a.mov(iced_x86::code_asm::qword_ptr(rbp - 8), rax).ok()?;
                a.leave().ok()?;
            }
            fill(&mut a, g, &mut written, &mut used, true)?;
            a.ret().ok()?;
        }
        4 => {
            // call through a register; the callee drops its frame with ret imm16
            let mut f = a.create_label();
            a.push(1i32).ok()?;
            a.push(2i32).ok()?;
            a.lea(rax, iced_x86::code_asm::qword_ptr(f)).ok()?;
            a.call(rax).ok()?;
            written.push(Register::RAX);
            fill(&mut a, g, &mut written, &mut used, true)?;
            a.hlt().ok()?;
            a.set_label(&mut f).ok()?;
            call_targets.push(f);
            a.mov(rdx, iced_x86::code_asm::qword_ptr(rsp + 8)).ok()?;
            a.add(rdx, iced_x86::code_asm::qword_ptr(rsp + 16)).ok()?;
            written.push(Register::RDX);
            a.ret_1(16).ok()?;
        }
        _ => {
            // string operations on the data block
            a.mov(ecx, g.rng.random_range(0u32..12)).ok()?;
            written.push(Register::RCX);
            if g.rng.random_bool(0.3) {
                a.std().ok()?;
            } else {
                a.cld().ok()?;
            }
            let i = match g.rng.random_range(0..9) {
                0 => Instruction::with_rep_movsb(64),
                1 => Instruction::with_rep_movsq(64),
                2 => Instruction::with_rep_stosb(64),
                3 => Instruction::with_rep_stosq(64),
                4 => Instruction::with_repe_cmpsb(64),
                5 => Instruction::with_repne_scasb(64),
                6 => Instruction::with_rep_lodsd(64),
                7 => Instruction::with_movsq(64, Register::None, iced_x86::RepPrefixKind::None),
                _ => Instruction::with_repe_cmpsq(64),
            }
            .ok()?;
            a.add_instruction(i).ok()?;
            written.push(Register::RSI);
            written.push(Register::RDI);
            a.cld().ok()?;
            fill(&mut a, g, &mut written, &mut used, true)?;
        }
    }
    a.hlt().ok()?;
    let res = a.assemble_options(CODE_BASE as u64, BlockEncoderOptions::NONE).ok()?;
    let code = res.inner.code_buffer.clone();
    for l in &call_targets {
        entries.push(res.label_ip(l).ok()? as u32);
    }
    let mut input = g.fresh_input(code.clone(), &[]);
    input.regs[Register::RSI as usize - Register::RAX as usize] = (DATA_BASE + 0x300) as u64;
    input.regs[Register::RDI as usize - Register::RAX as usize] = (DATA_BASE + 0x400) as u64;
    let mut dec = Decoder::with_ip(64, &code, CODE_BASE as u64, DecoderOptions::NONE);
    let instrs: Vec<Instruction> = dec.iter().collect();
    Some(Case64 { input, instrs, codes: used, entries })
}

pub const _SIZES: (fn(Register, u8) -> Register, usize, usize) = (sized, DATA_SIZE, STACK_SIZE);
