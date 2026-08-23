//! Random test-case generation from iced-x86 `Code` encodings.

use crate::proto::{CaseIn, DATA_BASE, STACK_SIZE};
use iced_x86::{Code, Encoder, Instruction, MemoryOperand, Mnemonic, OpCodeOperandKind, Register};
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// Mnemonics the integer lifter claims to support.
pub fn integer_mnemonics() -> Vec<Mnemonic> {
    use Mnemonic::*;
    vec![
        Mov, Movzx, Movsx, Lea, Push, Pop, Add, Adc, Sub, Sbb, Cmp, And, Or, Xor, Test, Inc, Dec, Neg, Not, Shl, Sal, Shr,
        Sar, Rol, Ror, Rcl, Rcr, Shld, Shrd, Imul, Mul, Div, Idiv, Cbw, Cwde, Cwd, Cdq, Xchg, Cmovo, Cmovno, Cmovb, Cmovae,
        Cmove, Cmovne, Cmovbe, Cmova, Cmovs, Cmovns, Cmovp, Cmovnp, Cmovl, Cmovge, Cmovle, Cmovg, Seto, Setno, Setb, Setae,
        Sete, Setne, Setbe, Seta, Sets, Setns, Setp, Setnp, Setl, Setge, Setle, Setg, Bt, Bts, Btr, Btc, Bsf, Bsr, Bswap,
        Xadd, Cmpxchg, Lahf, Sahf, Clc, Stc, Cmc, Cld, Std, Nop,
    ]
}

fn kind_ok(k: OpCodeOperandKind) -> bool {
    use OpCodeOperandKind::*;
    matches!(
        k,
        r8_reg | r8_or_mem | r8_opcode | r16_reg | r16_reg_mem | r16_or_mem | r16_rm | r16_opcode | r32_reg | r32_reg_mem | r32_or_mem | r32_rm
            | r32_opcode | imm8 | imm8sex16 | imm8sex32 | imm16 | imm32 | mem | mem_offs | al | ax | eax | cl | dx
            | imm8_const_1 | OpCodeOperandKind::None
    )
}

/// Codes usable in 32-bit mode whose operands the generator can fill.
pub fn supported_codes(mnemonics: &[Mnemonic]) -> Vec<Code> {
    let mut v = Vec::new();
    for code in Code::values() {
        let oc = code.op_code();
        if !oc.is_instruction() || !oc.mode32() || !mnemonics.contains(&code.mnemonic()) {
            continue;
        }
        // Skip 64-bit-only, segment, far and legacy forms.
        let name = format!("{code:?}");
        // 486-only cmpxchg encoding, VIA padlock, and bswap r16 (undefined result) are not Win32 code.
        if name.contains("r64") || name.contains("Sreg") || name.contains("_rm64") || name.contains("486") || name.contains("Xstore") || name == "Bswap_r16" {
            continue;
        }
        if (0..oc.op_count()).all(|n| kind_ok(oc.op_kind(n))) && oc.op_count() <= 3 {
            // `a16` addressing/operand forms are not interesting for Win32 code.
            if oc.address_size() == 16 || oc.operand_size() == 64 {
                continue;
            }
            v.push(code);
        }
    }
    v
}

pub struct Gen {
    pub rng: StdRng,
}

/// Registers whose initial value points at memory, by role.
const BASES: [Register; 4] = [Register::EBX, Register::ESI, Register::EDI, Register::EBP];
const INDEXES: [Register; 2] = [Register::ECX, Register::EDX];

#[derive(Clone)]
pub struct Case {
    pub input: CaseIn,
    pub instrs: Vec<Instruction>,
    pub codes: Vec<Code>,
    /// Additional function entry addresses inside the code block (call targets).
    pub entries: Vec<u32>,
}

impl Gen {
    pub fn new(seed: u64) -> Self {
        Gen { rng: StdRng::seed_from_u64(seed) }
    }

    fn reg(&mut self, size: u8, dest: bool, written: &[Register]) -> Register {
        let pool32: Vec<Register> = if dest {
            vec![Register::EAX, Register::ECX, Register::EDX]
        } else {
            [Register::EAX, Register::ECX, Register::EDX, Register::EBX, Register::ESI, Register::EDI, Register::EBP]
                .into_iter()
                .filter(|r| !written.contains(r) || !BASES.contains(r))
                .collect()
        };
        let r32 = pool32[self.rng.random_range(0..pool32.len())];
        match size {
            4 => r32,
            2 => Register::AX + (r32 as u32 - Register::EAX as u32),
            _ => {
                let low = Register::AL + (r32 as u32 - Register::EAX as u32);
                if matches!(r32, Register::EAX | Register::ECX | Register::EDX | Register::EBX) && self.rng.random_bool(0.3) {
                    low + 4 // AH/CH/DH/BH
                } else {
                    low
                }
            }
        }
    }

    fn mem(&mut self, written: &[Register], allow_index: bool) -> (MemoryOperand, Option<Register>) {
        let bases: Vec<Register> = BASES.iter().copied().filter(|b| !written.contains(b)).collect();
        if bases.is_empty() || self.rng.random_bool(0.1) {
            let disp = DATA_BASE + self.rng.random_range(0x40..0x7c0);
            return (MemoryOperand::with_displ(disp as u64, 4), None);
        }
        let base = bases[self.rng.random_range(0..bases.len())];
        let disp = self.rng.random_range(-0x80i64..0x80);
        let mut index = None;
        let mut mo = MemoryOperand::with_base_displ(base, disp);
        if allow_index && self.rng.random_bool(0.4) {
            let ix = INDEXES[self.rng.random_range(0..2)];
            if !written.contains(&ix) {
                let scale = [1u32, 2, 4, 8][self.rng.random_range(0..4)];
                mo = MemoryOperand::with_base_index_scale_displ_size(base, ix, scale, disp, 1);
                index = Some(ix);
            }
        }
        (mo, index)
    }

    /// Builds one random instruction for `code`, tracking register writes.
    fn instr(&mut self, code: Code, written: &mut Vec<Register>, index_regs: &mut Vec<Register>) -> Option<Instruction> {
        use OpCodeOperandKind::*;
        let oc = code.op_code();
        let allow_index = true;
        #[derive(Clone, Copy)]
        enum Op {
            R(Register),
            M(MemoryOperand),
            I(i64),
            None,
        }
        let mut ops = Vec::new();
        for n in 0..oc.op_count() {
            let dest = n == 0 && oc.op_count() > 1 || matches!(code.mnemonic(), Mnemonic::Inc | Mnemonic::Dec | Mnemonic::Neg | Mnemonic::Not | Mnemonic::Pop | Mnemonic::Bswap | Mnemonic::Seto);
            let op = match oc.op_kind(n) {
                r8_reg | r8_opcode => Op::R(self.reg(1, dest, written)),
                r16_reg | r16_reg_mem | r16_rm | r16_opcode => Op::R(self.reg(2, dest, written)),
                r32_reg | r32_reg_mem | r32_rm | r32_opcode => Op::R(self.reg(4, dest, written)),
                r8_or_mem | r16_or_mem | r32_or_mem => {
                    if self.rng.random_bool(0.5) {
                        let size = match oc.op_kind(n) { r8_or_mem => 1, r16_or_mem => 2, _ => 4 };
                        Op::R(self.reg(size, dest, written))
                    } else {
                        let (m, ix) = self.mem(written, allow_index);
                        if let Some(ix) = ix { index_regs.push(ix); }
                        Op::M(m)
                    }
                }
                mem => {
                    let (m, ix) = self.mem(written, allow_index);
                    if let Some(ix) = ix { index_regs.push(ix); }
                    Op::M(m)
                }
                mem_offs => Op::M(MemoryOperand::with_displ((DATA_BASE + self.rng.random_range(0x40..0x7c0)) as u64, 4)),
                imm8 => Op::I(self.rng.random_range(0..256)),
                imm8sex16 | imm8sex32 => Op::I(self.rng.random_range(-128..128)),
                imm16 => Op::I(self.rng.random_range(0..65536)),
                imm32 => Op::I(self.rng.random::<u32>() as i64),
                al => Op::R(Register::AL),
                ax => Op::R(Register::AX),
                eax => Op::R(Register::EAX),
                cl => Op::R(Register::CL),
                dx => Op::R(Register::DX),
                imm8_const_1 => Op::I(1),
                None => Op::None,
                _ => return Option::None,
            };
            ops.push(op);
        }
        // bt/bts/btr/btc [mem], reg: a random 32-bit bit offset addresses memory far away
        // (a fault on both sides tells nothing), so give it a small-valued register.
        if matches!(code.mnemonic(), Mnemonic::Bt | Mnemonic::Bts | Mnemonic::Btr | Mnemonic::Btc)
            && matches!(ops.first(), Some(Op::M(_)))
        {
            if let Some(Op::R(r)) = ops.get(1).copied() {
                if written.contains(&Register::ECX) {
                    return Option::None;
                }
                let small = if r.size() == 2 { Register::CX } else { Register::ECX };
                ops[1] = Op::R(small);
                index_regs.push(Register::ECX);
            }
        }
        // 16-bit shld/shrd with a count above 16 is undefined on Intel; keep counts in range.
        if matches!(code.mnemonic(), Mnemonic::Shld | Mnemonic::Shrd) && oc.operand_size() == 16 {
            match ops.get(2).copied() {
                Some(Op::I(i)) => ops[2] = Op::I(i & 15),
                Some(Op::R(_)) if written.contains(&Register::ECX) => return Option::None,
                Some(Op::R(_)) => index_regs.push(Register::ECX),
                _ => {}
            }
        }
        // Record destination writes (first operand, plus implicit ones).
        if let Some(Op::R(r)) = ops.first() {
            if oc.op_count() == 1 && matches!(code.mnemonic(), Mnemonic::Push | Mnemonic::Mul | Mnemonic::Imul | Mnemonic::Div | Mnemonic::Idiv) {
                // source only
            } else {
                written.push(r.full_register32());
            }
        }
        match code.mnemonic() {
            Mnemonic::Mul | Mnemonic::Imul | Mnemonic::Div | Mnemonic::Idiv | Mnemonic::Cwd | Mnemonic::Cdq | Mnemonic::Cbw | Mnemonic::Cwde => {
                written.push(Register::EAX);
                written.push(Register::EDX);
            }
            Mnemonic::Xchg | Mnemonic::Xadd => {
                if let Some(Op::R(r)) = ops.get(1) { written.push(r.full_register32()); }
            }
            Mnemonic::Cmpxchg => written.push(Register::EAX),
            Mnemonic::Pop => {
                if let Some(Op::R(r)) = ops.first() { written.push(r.full_register32()); }
            }
            Mnemonic::Lahf => written.push(Register::EAX),
            _ => {}
        }
        let build = |ops: &[Op]| -> Option<Instruction> {
            macro_rules! o {
                ($x:expr) => {
                    match $x {
                        Op::R(r) => OpArg::R(r),
                        Op::M(m) => OpArg::M(m),
                        Op::I(i) => OpArg::I(i),
                        Op::None => OpArg::I(0),
                    }
                };
            }
            match ops.len() {
                0 => Some(Instruction::with(code)),
                1 => with1(code, o!(ops[0])),
                2 => with2(code, o!(ops[0]), o!(ops[1])),
                3 => with3(code, o!(ops[0]), o!(ops[1]), o!(ops[2])),
                _ => unreachable!(),
            }
        };
        build(&ops)
    }

    pub fn case(&mut self, codes: &[Code], max_instrs: usize) -> Option<Case> {
        let n = self.rng.random_range(1..=max_instrs);
        let mut instrs = Vec::new();
        let mut used = Vec::new();
        let mut written = Vec::new();
        let mut index_regs = Vec::new();
        for _ in 0..n {
            let code = codes[self.rng.random_range(0..codes.len())];
            if let Some(i) = self.instr(code, &mut written, &mut index_regs) {
                instrs.push(i);
                used.push(code);
            }
        }
        if instrs.is_empty() {
            return None;
        }
        let mut enc = Encoder::new(32);
        let mut ip = crate::proto::CODE_BASE as u64;
        for i in instrs.iter_mut() {
            i.set_ip(ip);
            let len = enc.encode(i, ip).ok()?;
            ip += len as u64;
        }
        let mut code = enc.take_buffer();
        code.push(0xF4);
        let mut input = CaseIn::new(code);
        for r in 0..8 {
            input.regs[r] = self.rng.random();
        }
        input.regs[3] = DATA_BASE + 0x200;
        input.regs[6] = DATA_BASE + 0x300;
        input.regs[7] = DATA_BASE + 0x400;
        input.regs[5] = DATA_BASE + 0x500;
        input.regs[4] = 0x200 + self.rng.random_range(0..0x20) * 4;
        for ix in &index_regs {
            input.regs[*ix as usize - Register::EAX as usize] = self.rng.random_range(0..16);
        }
        // Small values exercise carries and zero results more often.
        if self.rng.random_bool(0.3) {
            input.regs[0] = self.rng.random_range(0..8) as u32 * 0x5555_5555;
        }
        input.eflags = self.rng.random::<u32>() & 0x8d5;
        self.rng.fill(&mut input.data[..]);
        self.rng.fill(&mut input.stack[..]);
        let _ = STACK_SIZE;
        Some(Case { input, instrs, codes: used, entries: Vec::new() })
    }

    /// One random instruction for use inside a larger program; `written` tracks registers
    /// the program has already changed (so memory bases stay valid).
    pub fn filler(&mut self, codes: &[Code], written: &mut Vec<Register>, allow_stack: bool) -> Option<(Instruction, Code)> {
        let mut index_regs = Vec::new();
        for _ in 0..8 {
            let code = codes[self.rng.random_range(0..codes.len())];
            if !allow_stack && matches!(code.mnemonic(), Mnemonic::Push | Mnemonic::Pop) {
                continue;
            }
            if let Some(i) = self.instr(code, written, &mut index_regs) {
                if index_regs.is_empty() {
                    return Some((i, code));
                }
                index_regs.clear();
            }
        }
        None
    }

    /// Initial state with the standard base registers and random data/stack contents.
    pub fn fresh_input(&mut self, code: Vec<u8>) -> CaseIn {
        let mut input = CaseIn::new(code);
        for r in 0..8 {
            input.regs[r] = self.rng.random();
        }
        input.regs[3] = DATA_BASE + 0x200;
        input.regs[6] = DATA_BASE + 0x300;
        input.regs[7] = DATA_BASE + 0x400;
        input.regs[5] = DATA_BASE + 0x500;
        input.regs[4] = 0x200 + self.rng.random_range(0..0x20) * 4;
        input.eflags = self.rng.random::<u32>() & 0x8d5;
        self.rng.fill(&mut input.data[..]);
        self.rng.fill(&mut input.stack[..]);
        input
    }
}

#[derive(Clone, Copy)]
enum OpArg {
    R(Register),
    M(MemoryOperand),
    I(i64),
}

fn with1(code: Code, a: OpArg) -> Option<Instruction> {
    match a {
        OpArg::R(r) => Instruction::with1(code, r).ok(),
        OpArg::M(m) => Instruction::with1(code, m).ok(),
        OpArg::I(i) => Instruction::with1(code, i as i32).ok(),
    }
}

fn with2(code: Code, a: OpArg, b: OpArg) -> Option<Instruction> {
    match (a, b) {
        (OpArg::R(x), OpArg::R(y)) => Instruction::with2(code, x, y).ok(),
        (OpArg::R(x), OpArg::M(y)) => Instruction::with2(code, x, y).ok(),
        (OpArg::R(x), OpArg::I(y)) => Instruction::with2(code, x, y as i32).ok(),
        (OpArg::M(x), OpArg::R(y)) => Instruction::with2(code, x, y).ok(),
        (OpArg::M(x), OpArg::I(y)) => Instruction::with2(code, x, y as i32).ok(),
        _ => None,
    }
}

fn with3(code: Code, a: OpArg, b: OpArg, c: OpArg) -> Option<Instruction> {
    match (a, b, c) {
        (OpArg::R(x), OpArg::R(y), OpArg::I(z)) => Instruction::with3(code, x, y, z as i32).ok(),
        (OpArg::R(x), OpArg::M(y), OpArg::I(z)) => Instruction::with3(code, x, y, z as i32).ok(),
        (OpArg::R(x), OpArg::R(y), OpArg::R(z)) => Instruction::with3(code, x, y, z).ok(),
        (OpArg::M(x), OpArg::R(y), OpArg::I(z)) => Instruction::with3(code, x, y, z as i32).ok(),
        (OpArg::M(x), OpArg::R(y), OpArg::R(z)) => Instruction::with3(code, x, y, z).ok(),
        _ => None,
    }
}
