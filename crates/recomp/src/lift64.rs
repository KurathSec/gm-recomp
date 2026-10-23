//! x86-64 instruction semantics (docs/x64.md), next to `lift.rs`, whose control flow,
//! lazy flags and SSE code 64-bit functions share.
//!
//! A 64-bit function's registers are its sixteen i64 parameters (locals 0..15: rax..rdi,
//! r8..r15); the 32-bit layout's other locals follow them (`LBASE64`), then the i64
//! locals below. Integer values are i64, zero-extended from their operand width: a
//! 32-bit write zero-extends into the full register, 8- and 16-bit writes keep the
//! rest. Memory is addressed by the low 32 bits of the effective address (`addr_x64`),
//! while `lea`, return addresses and RIP-relative values keep all 64 bits. Pending
//! flags hold their operands in `X_FA`/`X_FB` and the result in `X_FR`, masked to the
//! operation's width.

use crate::abi::{Global, RtFunc, trap};
use crate::flags::{self, FlagOp};
use crate::lift::{FuncEmit, L_D0, L_FC, L_FLAG, L_REG, L_T0, L_T1, L_T2, LOCALS, mem0};
use iced_x86::{Code, ConditionCode, Instruction, Mnemonic, OpKind, Register};
use std::borrow::Cow;
use wasm_encoder::{BlockType, Instruction as W, MemArg, ValType};

/// The 32-bit layout's locals from `L_FLAG` on lie this much further on: after sixteen
/// register parameters instead of eight.
pub(crate) const LBASE64: u32 = 8;
/// i64 locals after those: the pending flag operation's operands and result.
pub(crate) const X_FA: u32 = 47;
pub(crate) const X_FB: u32 = 48;
pub(crate) const X_FR: u32 = 49;
/// Scratch.
pub(crate) const X_T0: u32 = 50;
pub(crate) const X_T1: u32 = 51;
pub(crate) const X_T2: u32 = 52;
pub(crate) const X_T3: u32 = 53;
pub(crate) const X_T4: u32 = 54;
pub(crate) const X_T5: u32 = 55;
pub(crate) const X_T6: u32 = 56;
/// `pop64_lo_to`'s.
pub(crate) const X_T7: u32 = 57;
/// The return address the function was entered with (functions with `jmp r/m64`).
pub(crate) const X_RA: u32 = 58;
/// `write_reg32_x64`'s.
pub(crate) const X_W: u32 = 59;

/// Locals after the parameters (the 32-bit layout's, then thirteen i64).
pub(crate) fn locals64() -> Vec<(u32, ValType)> {
    let mut v = LOCALS.to_vec();
    v.push((13, ValType::I64));
    v
}

const RAX: u32 = 0;
const RCX: u32 = 1;
const RDX: u32 = 2;
const RBX: u32 = 3;
const RSP: u32 = 4;
const RBP: u32 = 5;
const RSI: u32 = 6;
const RDI: u32 = 7;

/// The register local (0..15) a general register lives in.
fn gpr(r: Register) -> u32 {
    r.full_register() as u32 - Register::RAX as u32
}

fn is_high8(r: Register) -> bool {
    matches!(r, Register::AH | Register::CH | Register::DH | Register::BH)
}

pub(crate) fn mask64(w: u8) -> u64 {
    if w >= 64 { u64::MAX } else { (1u64 << w) - 1 }
}

/// Operand `n`'s width in bits.
fn width(instr: &Instruction, n: u32) -> u8 {
    match instr.op_kind(n) {
        OpKind::Register => instr.op_register(n).size() as u8 * 8,
        OpKind::Memory => instr.memory_size().size() as u8 * 8,
        OpKind::Immediate8 | OpKind::Immediate8_2nd => 8,
        OpKind::Immediate16 | OpKind::Immediate8to16 => 16,
        OpKind::Immediate64 | OpKind::Immediate8to64 | OpKind::Immediate32to64 => 64,
        _ => 32,
    }
}

/// The accumulator of a width.
fn acc(w: u8) -> Register {
    match w {
        8 => Register::AL,
        16 => Register::AX,
        32 => Register::EAX,
        _ => Register::RAX,
    }
}

/// Where a destination operand lives once its address is computed.
#[derive(Clone, Copy)]
enum D {
    Reg(Register),
    /// Address (low 32 bits) in L_T0.
    Mem,
    None,
}

impl<'a> FuncEmit<'a> {
    fn k(&mut self, v: u64) {
        self.f.instructions().i64_const(v as i64);
    }

    // ---- registers ---------------------------------------------------------------

    /// Pushes register `r` zero-extended to i64.
    pub(crate) fn rget(&mut self, r: Register) {
        self.lget(gpr(r));
        match r.size() {
            8 => {}
            4 => {
                self.i(W::I32WrapI64);
                self.i(W::I64ExtendI32U);
            }
            2 => {
                self.k(0xffff);
                self.i(W::I64And);
            }
            _ => {
                if is_high8(r) {
                    self.k(8);
                    self.i(W::I64ShrU);
                }
                self.k(0xff);
                self.i(W::I64And);
            }
        }
    }

    /// Writes the low bytes of i64 local `src` to register `r`: a 32-bit write
    /// zero-extends into the full register, 8- and 16-bit writes keep the rest.
    pub(crate) fn rset_from(&mut self, r: Register, src: u32) {
        let l = gpr(r);
        match r.size() {
            8 => {
                self.lget(src);
                self.lset(l);
            }
            4 => {
                self.lget(src);
                self.i(W::I32WrapI64);
                self.i(W::I64ExtendI32U);
                self.lset(l);
            }
            2 => {
                self.lget(l);
                self.k(!0xffff);
                self.i(W::I64And);
                self.lget(src);
                self.k(0xffff);
                self.i(W::I64And);
                self.i(W::I64Or);
                self.lset(l);
            }
            _ => {
                let high = is_high8(r);
                self.lget(l);
                self.k(if high { !0xff00 } else { !0xff });
                self.i(W::I64And);
                self.lget(src);
                self.k(0xff);
                self.i(W::I64And);
                if high {
                    self.k(8);
                    self.i(W::I64Shl);
                }
                self.i(W::I64Or);
                self.lset(l);
            }
        }
    }

    /// `read_reg` in 64-bit code: the register's low 32 bits or less, as i32.
    pub(crate) fn read_reg32_x64(&mut self, r: Register) {
        self.lget(gpr(r));
        self.i(W::I32WrapI64);
        match r.size() {
            8 | 4 => {}
            2 => {
                self.c(0xffff);
                self.i(W::I32And);
            }
            _ => {
                if is_high8(r) {
                    self.c(8);
                    self.i(W::I32ShrU);
                }
                self.c(0xff);
                self.i(W::I32And);
            }
        }
    }

    /// `write_reg_from` in 64-bit code: i32 local `src` into `r` (a 64-bit register
    /// gets it zero-extended).
    pub(crate) fn write_reg32_x64(&mut self, r: Register, src: u32) {
        self.get(src);
        self.i(W::I64ExtendI32U);
        self.lset(X_W);
        self.rset_from(r, X_W);
    }

    // ---- memory ------------------------------------------------------------------

    /// Pushes the low 32 bits of the memory operand's effective address (i32): what
    /// guest memory is addressed by. The sum's low half is the sum of the low halves.
    pub(crate) fn addr_x64(&mut self, instr: &Instruction) {
        let base = instr.memory_base();
        let index = instr.memory_index();
        if matches!(base, Register::RIP | Register::EIP) {
            // (iced gives the absolute target)
            self.c(instr.memory_displacement64() as u32);
        } else {
            let mut terms = 0;
            if base != Register::None {
                self.lget(gpr(base));
                self.i(W::I32WrapI64);
                terms += 1;
            }
            if index != Register::None {
                self.lget(gpr(index));
                self.i(W::I32WrapI64);
                let s = instr.memory_index_scale();
                if s > 1 {
                    self.c(s.trailing_zeros());
                    self.i(W::I32Shl);
                }
                if terms > 0 {
                    self.i(W::I32Add);
                }
                terms += 1;
            }
            let disp = instr.memory_displacement64() as u32;
            if disp != 0 || terms == 0 {
                self.c(disp);
                if terms > 0 {
                    self.i(W::I32Add);
                }
            }
        }
        // FS has base 0 in 64-bit code; GS addresses the TEB.
        if instr.segment_prefix() == Register::GS {
            self.i(W::GlobalGet(Global::GsBase as u32));
            self.i(W::I32Add);
        }
    }

    /// Pushes the full effective address (i64) of the memory operand: `lea`.
    fn lea_value(&mut self, instr: &Instruction) {
        let base = instr.memory_base();
        let index = instr.memory_index();
        match base {
            Register::RIP => return self.k(instr.memory_displacement64()),
            Register::EIP => return self.k(instr.memory_displacement64() as u32 as u64),
            _ => {}
        }
        let a32 = base.is_gpr32() || index.is_gpr32();
        let mut terms = 0;
        if base != Register::None {
            self.lget(gpr(base));
            terms += 1;
        }
        if index != Register::None {
            self.lget(gpr(index));
            let s = instr.memory_index_scale();
            if s > 1 {
                self.k(s.trailing_zeros() as u64);
                self.i(W::I64Shl);
            }
            if terms > 0 {
                self.i(W::I64Add);
            }
            terms += 1;
        }
        let disp = instr.memory_displacement64();
        if disp != 0 || terms == 0 {
            self.k(disp);
            if terms > 0 {
                self.i(W::I64Add);
            }
        }
        if a32 {
            self.i(W::I32WrapI64);
            self.i(W::I64ExtendI32U);
        }
    }

    /// [address (i32)] -> [value (i64)], `bytes` wide, zero-extended.
    fn load64(&mut self, bytes: usize) {
        self.i(match bytes {
            1 => W::I64Load8U(mem0()),
            2 => W::I64Load16U(mem0()),
            4 => W::I64Load32U(mem0()),
            8 => W::I64Load(mem0()),
            _ => return self.bad_width(bytes as u8 * 8),
        });
    }

    /// [address (i32), value (i64)] -> [], the value's low `bytes`.
    fn store64(&mut self, bytes: usize) {
        self.i(match bytes {
            1 => W::I64Store8(mem0()),
            2 => W::I64Store16(mem0()),
            4 => W::I64Store32(mem0()),
            8 => W::I64Store(mem0()),
            _ => return self.bad_width(bytes as u8 * 8),
        });
    }

    /// Pushes operand `n` (i64, zero-extended from its width; an immediate takes the
    /// width of operand 0, sign-extended there as x86 does).
    fn rop(&mut self, instr: &Instruction, n: u32) {
        match instr.op_kind(n) {
            OpKind::Register => self.rget(instr.op_register(n)),
            OpKind::Memory => {
                self.addr_x64(instr);
                self.load64(instr.memory_size().size());
            }
            OpKind::Immediate8_2nd => self.k(instr.immediate8_2nd() as u64),
            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => {
                let w = width(instr, 0).max(8);
                self.k(instr.immediate(n) & mask64(w));
            }
            k => panic!("unsupported operand kind {k:?} at {:#x}", self.ip),
        }
    }

    fn prep(&mut self, instr: &Instruction, n: u32) -> D {
        match instr.op_kind(n) {
            OpKind::Register => D::Reg(instr.op_register(n)),
            OpKind::Memory => {
                self.addr_x64(instr);
                self.set(L_T0);
                D::Mem
            }
            _ => D::None,
        }
    }

    fn rdst(&mut self, d: D, w: u8) {
        match d {
            D::Reg(r) => self.rget(r),
            D::Mem => {
                self.get(L_T0);
                self.load64(w as usize / 8);
            }
            D::None => self.k(0),
        }
    }

    fn wdst(&mut self, d: D, w: u8, src: u32) {
        match d {
            D::Reg(r) => self.rset_from(r, src),
            D::Mem => {
                self.get(L_T0);
                self.lget(src);
                self.store64(w as usize / 8);
            }
            D::None => {}
        }
    }

    fn sext64(&mut self, w: u8) {
        match w {
            8 => self.i(W::I64Extend8S),
            16 => self.i(W::I64Extend16S),
            32 => self.i(W::I64Extend32S),
            _ => {}
        }
    }

    /// Masks the i64 on the stack to `w` bits.
    fn mask_to(&mut self, w: u8) {
        if w < 64 {
            self.k(mask64(w));
            self.i(W::I64And);
        }
    }

    // ---- stack -------------------------------------------------------------------

    pub(crate) fn push64_const(&mut self, v: u64) {
        self.lget(RSP);
        self.k(8);
        self.i(W::I64Sub);
        self.ltee(RSP);
        self.i(W::I32WrapI64);
        self.k(v);
        self.i(W::I64Store(mem0()));
    }

    fn push64_from(&mut self, src: u32) {
        self.lget(RSP);
        self.k(8);
        self.i(W::I64Sub);
        self.ltee(RSP);
        self.i(W::I32WrapI64);
        self.lget(src);
        self.i(W::I64Store(mem0()));
    }

    /// Pops a qword into i64 local `dst` (not rsp itself).
    fn pop64_to(&mut self, dst: u32) {
        self.lget(RSP);
        self.i(W::I32WrapI64);
        self.i(W::I64Load(mem0()));
        self.lset(dst);
        self.lget(RSP);
        self.k(8);
        self.i(W::I64Add);
        self.lset(RSP);
    }

    /// Pops a qword; its low 32 bits go to i32 local `dst` (a return address).
    pub(crate) fn pop64_lo_to(&mut self, dst: u32) {
        self.pop64_to(X_T7);
        self.lget(X_T7);
        self.i(W::I32WrapI64);
        self.set(dst);
    }

    // ---- flags -------------------------------------------------------------------

    /// `flag` of a pending 64-bit-code operation: from the i64 operands.
    pub(crate) fn flag_x64(&mut self, fl: u8, op: FlagOp, w: u8) {
        let msb = |me: &mut Self, l: u32| {
            me.lget(l);
            me.k(w as u64 - 1);
            me.i(W::I64ShrU);
            me.i(W::I32WrapI64);
            me.c(1);
            me.i(W::I32And);
        };
        match fl {
            flags::ZF => {
                self.lget(X_FR);
                self.i(W::I64Eqz);
            }
            flags::SF => msb(self, X_FR),
            flags::PF => {
                self.lget(X_FR);
                self.i(W::I32WrapI64);
                self.c(0xff);
                self.i(W::I32And);
                self.i(W::I32Popcnt);
                self.c(1);
                self.i(W::I32And);
                self.c(1);
                self.i(W::I32Xor);
            }
            flags::AF => match op {
                FlagOp::Add | FlagOp::Adc | FlagOp::Sub | FlagOp::Sbb | FlagOp::Inc | FlagOp::Dec => {
                    self.lget(X_FA);
                    self.lget(X_FB);
                    self.i(W::I64Xor);
                    self.lget(X_FR);
                    self.i(W::I64Xor);
                    self.i(W::I32WrapI64);
                    self.c(4);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                _ => self.c(0),
            },
            flags::CF => match op {
                FlagOp::Add => {
                    self.lget(X_FR);
                    self.lget(X_FA);
                    self.i(W::I64LtU);
                }
                FlagOp::Adc => {
                    // carry-in ? res <= a : res < a
                    self.lget(X_FR);
                    self.lget(X_FA);
                    self.i(W::I64LeU);
                    self.lget(X_FR);
                    self.lget(X_FA);
                    self.i(W::I64LtU);
                    self.get(L_FC);
                    self.i(W::Select);
                }
                FlagOp::Sub => {
                    self.lget(X_FA);
                    self.lget(X_FB);
                    self.i(W::I64LtU);
                }
                FlagOp::Sbb => {
                    self.lget(X_FA);
                    self.lget(X_FB);
                    self.i(W::I64LeU);
                    self.lget(X_FA);
                    self.lget(X_FB);
                    self.i(W::I64LtU);
                    self.get(L_FC);
                    self.i(W::Select);
                }
                FlagOp::Logic => self.c(0),
                FlagOp::Inc | FlagOp::Dec => self.get(L_FLAG),
                FlagOp::Shl => {
                    // last bit shifted out = bit (w - n) of a, 0 when n > w
                    self.lget(X_FA);
                    self.k(w as u64);
                    self.lget(X_FB);
                    self.i(W::I64Sub);
                    self.i(W::I64ShrU);
                    self.i(W::I32WrapI64);
                    self.c(1);
                    self.i(W::I32And);
                    self.c(0);
                    self.lget(X_FB);
                    self.k(w as u64);
                    self.i(W::I64LeU);
                    self.i(W::Select);
                }
                FlagOp::Shr | FlagOp::Sar => {
                    // bit (n - 1) of the (sign- or zero-extended) source
                    self.lget(X_FA);
                    if op == FlagOp::Sar {
                        self.sext64(w);
                    }
                    self.lget(X_FB);
                    self.k(1);
                    self.i(W::I64Sub);
                    self.i(W::I64ShrU);
                    self.i(W::I32WrapI64);
                    self.c(1);
                    self.i(W::I32And);
                }
            },
            flags::OF => match op {
                FlagOp::Add | FlagOp::Adc => {
                    self.lget(X_FA);
                    self.lget(X_FR);
                    self.i(W::I64Xor);
                    self.lget(X_FB);
                    self.lget(X_FR);
                    self.i(W::I64Xor);
                    self.i(W::I64And);
                    self.lset(X_T7);
                    msb(self, X_T7);
                }
                FlagOp::Sub | FlagOp::Sbb => {
                    self.lget(X_FA);
                    self.lget(X_FB);
                    self.i(W::I64Xor);
                    self.lget(X_FA);
                    self.lget(X_FR);
                    self.i(W::I64Xor);
                    self.i(W::I64And);
                    self.lset(X_T7);
                    msb(self, X_T7);
                }
                FlagOp::Logic | FlagOp::Sar => self.c(0),
                FlagOp::Inc => {
                    self.lget(X_FR);
                    self.k(1 << (w - 1));
                    self.i(W::I64Eq);
                }
                FlagOp::Dec => {
                    self.lget(X_FR);
                    self.k((1u64 << (w - 1)) - 1);
                    self.i(W::I64Eq);
                }
                FlagOp::Shl => {
                    // defined for n == 1: msb(res) ^ msb(a); otherwise 0
                    self.lget(X_FR);
                    self.lget(X_FA);
                    self.i(W::I64Xor);
                    self.lset(X_T7);
                    msb(self, X_T7);
                    self.c(0);
                    self.lget(X_FB);
                    self.k(1);
                    self.i(W::I64Eq);
                    self.i(W::Select);
                }
                FlagOp::Shr => {
                    msb(self, X_FA);
                    self.c(0);
                    self.lget(X_FB);
                    self.k(1);
                    self.i(W::I64Eq);
                    self.i(W::Select);
                }
            },
            _ => unreachable!(),
        }
    }

    /// `cond` straight from a pending compare's operands; false if none is pending.
    pub(crate) fn cond_x64_fast(&mut self, cc: ConditionCode) -> bool {
        use ConditionCode as CC;
        let Some((FlagOp::Sub, w)) = self.pending else { return false };
        let unsigned = |me: &mut Self, op: W<'static>| {
            me.lget(X_FA);
            me.lget(X_FB);
            me.i(op);
        };
        let signed = |me: &mut Self, op: W<'static>| {
            me.lget(X_FA);
            me.sext64(w);
            me.lget(X_FB);
            me.sext64(w);
            me.i(op);
        };
        match cc {
            CC::b => unsigned(self, W::I64LtU),
            CC::ae => unsigned(self, W::I64GeU),
            CC::be => unsigned(self, W::I64LeU),
            CC::a => unsigned(self, W::I64GtU),
            CC::e => {
                self.lget(X_FR);
                self.i(W::I64Eqz);
            }
            CC::ne => {
                self.lget(X_FR);
                self.k(0);
                self.i(W::I64Ne);
            }
            CC::l => signed(self, W::I64LtS),
            CC::ge => signed(self, W::I64GeS),
            CC::le => signed(self, W::I64LeS),
            CC::g => signed(self, W::I64GtS),
            _ => return false,
        }
        true
    }

    /// Materialized compare flags of X_FA - X_FB (string compares).
    fn cmp_flags64(&mut self, w: u8) {
        self.lget(X_FA);
        self.lget(X_FB);
        self.i(W::I64Sub);
        self.mask_to(w);
        self.lset(X_FR);
        self.pending = Some((FlagOp::Sub, w));
        self.materialize(flags::ALL);
    }

    // ---- control flow -------------------------------------------------------------

    /// The condition of `jrcxz`/`jecxz` and `loop`/`loope`/`loopne` (which count rcx,
    /// or ecx with a 32-bit address size).
    pub(crate) fn rcx_cond_x64(&mut self, instr: &Instruction) {
        let ecx = matches!(instr.code(), Code::Jecxz_rel8_64 | Code::Loop_rel8_64_ECX | Code::Loope_rel8_64_ECX | Code::Loopne_rel8_64_ECX);
        match instr.mnemonic() {
            Mnemonic::Jrcxz | Mnemonic::Jecxz => {
                self.lget(RCX);
                if ecx {
                    self.i(W::I32WrapI64);
                    self.i(W::I32Eqz);
                } else {
                    self.i(W::I64Eqz);
                }
            }
            m => {
                self.lget(RCX);
                self.k(1);
                self.i(W::I64Sub);
                if ecx {
                    self.i(W::I32WrapI64);
                    self.i(W::I64ExtendI32U);
                }
                self.ltee(RCX);
                self.k(0);
                self.i(W::I64Ne);
                if m != Mnemonic::Loop {
                    self.flag(flags::ZF);
                    if m == Mnemonic::Loopne {
                        self.i(W::I32Eqz);
                    }
                    self.i(W::I32And);
                }
            }
        }
    }

    /// A switch's `jmp reg` with its table (`table_index_reg`): the index register
    /// selects the case; an index outside the table, or a block of another shape,
    /// jumps to the register's address.
    pub(crate) fn table_x64(&mut self, instr: &Instruction, targets: &[u32]) {
        let cfg = self.cfg;
        let block = &cfg.blocks[&self.order[self.cur_block]];
        if let Some(sw) = x86::switch64(&block.instrs).filter(|sw| sw.index_kept) {
            let ix = sw.index;
            let n = targets.len() as u32;
            self.i(W::Block(BlockType::Empty));
            for _ in 0..n {
                self.i(W::Block(BlockType::Empty));
            }
            // (an index above 2^32 must not wrap into the table)
            self.rget(ix);
            self.i(W::I32WrapI64);
            self.c(n);
            self.rget(ix);
            self.k(n as u64);
            self.i(W::I64LtU);
            self.i(W::Select);
            self.i(W::BrTable(Cow::Owned((0..n).collect()), n));
            for (k, case) in targets.iter().enumerate() {
                self.i(W::End);
                let remaining = n - 1 - k as u32;
                self.goto_within(*case, remaining + 1);
            }
            self.i(W::End);
        }
        self.rop(instr, 0);
        self.i(W::I32WrapI64);
        self.set(L_T1);
        self.tail_dynamic();
    }

    /// `jmp r/m64` (see the 32-bit `Term::IndirectTail`): to the address the function
    /// was entered to return to, a return; elsewhere a tail call.
    pub(crate) fn indirect_tail_x64(&mut self, instr: &Instruction) {
        self.rop(instr, 0);
        self.lset(X_T0);
        self.materialize(flags::ALL);
        self.lget(X_T0);
        self.lget(X_RA);
        self.i(W::I64Eq);
        self.i(W::If(BlockType::Empty));
        self.lget(X_T0);
        self.i(W::I32WrapI64);
        self.i(W::GlobalSet(Global::Eip as u32));
        self.spill_flags();
        self.ret_regs();
        self.i(W::End);
        self.lget(X_T0);
        self.i(W::I32WrapI64);
        self.set(L_T1);
        self.tail_dynamic();
    }

    // ---- instructions -------------------------------------------------------------

    pub(crate) fn emit_instr_x64(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        match instr.mnemonic() {
            Nop | Pause | Lfence | Mfence | Sfence | Prefetcht0 | Prefetcht1 | Prefetcht2 | Prefetchnta | Prefetchw | Endbr64 | Endbr32
            | Wait => {}
            Mov => {
                if instr.op0_kind() == OpKind::Register && !instr.op0_register().is_gpr() {
                    return self.unimplemented(instr);
                }
                if instr.op1_kind() == OpKind::Register && instr.op1_register().is_segment_register() {
                    // the selectors of a 64-bit process
                    let sel = match instr.op1_register() {
                        Register::CS => 0x33,
                        Register::FS => 0x53,
                        _ => 0x2B,
                    };
                    let w = if instr.op0_kind() == OpKind::Register { width(instr, 0) } else { 16 };
                    let d = self.prep(instr, 0);
                    self.k(sel);
                    self.lset(X_T1);
                    return self.wdst(d, w, X_T1);
                }
                if instr.op1_kind() == OpKind::Register && !instr.op1_register().is_gpr() {
                    return self.unimplemented(instr);
                }
                let w = width(instr, 0);
                let d = self.prep(instr, 0);
                self.rop(instr, 1);
                self.lset(X_T1);
                self.wdst(d, w, X_T1);
            }
            Movzx | Movsx | Movsxd => {
                let d = self.prep(instr, 0);
                let sw = width(instr, 1);
                self.rop(instr, 1);
                if instr.mnemonic() != Movzx {
                    self.sext64(sw);
                }
                self.lset(X_T1);
                self.wdst(d, width(instr, 0), X_T1);
            }
            Lea => {
                self.lea_value(instr);
                self.lset(X_T1);
                self.rset_from(instr.op0_register(), X_T1);
            }
            Push => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register() {
                    return self.unimplemented(instr);
                }
                // push rsp pushes the value before the decrement: read first
                self.rop(instr, 0);
                self.lset(X_T1);
                if width(instr, 0) == 16 {
                    self.lget(RSP);
                    self.k(2);
                    self.i(W::I64Sub);
                    self.ltee(RSP);
                    self.i(W::I32WrapI64);
                    self.lget(X_T1);
                    self.i(W::I64Store16(mem0()));
                } else {
                    self.push64_from(X_T1);
                }
            }
            Pop => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register() {
                    return self.unimplemented(instr);
                }
                let w = width(instr, 0);
                if w == 16 {
                    self.lget(RSP);
                    self.i(W::I32WrapI64);
                    self.i(W::I64Load16U(mem0()));
                    self.lset(X_T1);
                    self.lget(RSP);
                    self.k(2);
                    self.i(W::I64Add);
                    self.lset(RSP);
                } else {
                    self.pop64_to(X_T1);
                }
                // a memory destination's address is computed with rsp updated
                let d = self.prep(instr, 0);
                self.wdst(d, w, X_T1);
            }
            Add => self.alu64(instr, FlagOp::Add, W::I64Add, true),
            Adc => self.alu64(instr, FlagOp::Adc, W::I64Add, true),
            Sub => self.alu64(instr, FlagOp::Sub, W::I64Sub, true),
            Sbb => self.alu64(instr, FlagOp::Sbb, W::I64Sub, true),
            Cmp => self.alu64(instr, FlagOp::Sub, W::I64Sub, false),
            And => self.alu64(instr, FlagOp::Logic, W::I64And, true),
            Or => self.alu64(instr, FlagOp::Logic, W::I64Or, true),
            Xor => self.alu64(instr, FlagOp::Logic, W::I64Xor, true),
            Test => self.alu64(instr, FlagOp::Logic, W::I64And, false),
            Inc | Dec => {
                let w = width(instr, 0);
                // CF survives: in its local before the result replaces the pending op
                self.materialize(flags::ALL);
                let d = self.prep(instr, 0);
                self.rdst(d, w);
                self.lset(X_FA);
                self.k(1);
                self.lset(X_FB);
                self.lget(X_FA);
                self.k(1);
                self.i(if instr.mnemonic() == Inc { W::I64Add } else { W::I64Sub });
                self.mask_to(w);
                self.lset(X_FR);
                self.wdst(d, w, X_FR);
                self.set_pending(if instr.mnemonic() == Inc { FlagOp::Inc } else { FlagOp::Dec }, w);
            }
            Neg => {
                let w = width(instr, 0);
                let d = self.prep(instr, 0);
                self.k(0);
                self.lset(X_FA);
                self.rdst(d, w);
                self.lset(X_FB);
                self.k(0);
                self.lget(X_FB);
                self.i(W::I64Sub);
                self.mask_to(w);
                self.lset(X_FR);
                self.wdst(d, w, X_FR);
                self.set_pending(FlagOp::Sub, w);
            }
            Not => {
                let w = width(instr, 0);
                let d = self.prep(instr, 0);
                self.rdst(d, w);
                self.k(mask64(w));
                self.i(W::I64Xor);
                self.lset(X_T1);
                self.wdst(d, w, X_T1);
            }
            Shl | Sal | Shr | Sar => self.shift64(instr),
            Rol | Ror | Rcl | Rcr => self.rotate64(instr),
            Shld | Shrd => self.double_shift64(instr),
            Imul => self.imul64(instr),
            Mul => self.mul64(instr),
            Div | Idiv => self.div64(instr),
            Cbw | Cwde | Cdqe => {
                let (from, to) = match instr.mnemonic() {
                    Cbw => (Register::AL, Register::AX),
                    Cwde => (Register::AX, Register::EAX),
                    _ => (Register::EAX, Register::RAX),
                };
                self.rget(from);
                self.sext64(from.size() as u8 * 8);
                self.lset(X_T1);
                self.rset_from(to, X_T1);
            }
            Cwd | Cdq | Cqo => {
                let (from, to) = match instr.mnemonic() {
                    Cwd => (Register::AX, Register::DX),
                    Cdq => (Register::EAX, Register::EDX),
                    _ => (Register::RAX, Register::RDX),
                };
                self.rget(from);
                self.sext64(from.size() as u8 * 8);
                self.k(63);
                self.i(W::I64ShrS);
                self.lset(X_T1);
                self.rset_from(to, X_T1);
            }
            Xchg => {
                let w = width(instr, 0);
                let d = self.prep(instr, 0);
                self.rdst(d, w);
                self.lset(X_T1);
                self.rop(instr, 1);
                self.lset(X_T2);
                self.wdst(d, w, X_T2);
                self.rset_from(instr.op1_register(), X_T1);
            }
            Cmovo | Cmovno | Cmovb | Cmovae | Cmove | Cmovne | Cmovbe | Cmova | Cmovs | Cmovns | Cmovp | Cmovnp | Cmovl
            | Cmovge | Cmovle | Cmovg => {
                // the destination is written either way (a 32-bit one zero-extends)
                let r = instr.op0_register();
                self.rop(instr, 1);
                self.rget(r);
                self.cond(instr.condition_code());
                self.i(W::Select);
                self.lset(X_T1);
                self.rset_from(r, X_T1);
            }
            Seto | Setno | Setb | Setae | Sete | Setne | Setbe | Seta | Sets | Setns | Setp | Setnp | Setl | Setge | Setle
            | Setg => {
                let d = self.prep(instr, 0);
                self.cond(instr.condition_code());
                self.i(W::I64ExtendI32U);
                self.lset(X_T1);
                self.wdst(d, 8, X_T1);
            }
            Call => match instr.op0_kind() {
                OpKind::NearBranch64 | OpKind::NearBranch32 => self.call_static(instr.near_branch_target() as u32),
                OpKind::Register | OpKind::Memory => {
                    // (the target is read before the return address goes on the stack)
                    self.rop(instr, 0);
                    self.i(W::I32WrapI64);
                    self.set(L_T1);
                    self.call_dynamic();
                }
                _ => self.unimplemented(instr),
            },
            Leave => {
                self.lget(RBP);
                self.lset(RSP);
                self.pop64_to(X_T1);
                self.lget(X_T1);
                self.lset(RBP);
            }
            Enter => {
                if instr.immediate8_2nd() != 0 || width(instr, 0) == 16 && instr.code() != Code::Enterq_imm16_imm8 {
                    return self.unimplemented(instr);
                }
                self.push64_from(RBP);
                self.lget(RSP);
                self.lset(RBP);
                self.lget(RSP);
                self.k(instr.immediate16() as u64);
                self.i(W::I64Sub);
                self.lset(RSP);
            }
            Bt | Bts | Btr | Btc => self.bit_test64(instr),
            Bsf | Bsr => {
                // a zero source sets ZF and leaves the destination as it was
                let r = instr.op0_register();
                self.rop(instr, 1);
                self.lset(X_T1);
                self.materialize(flags::ALL);
                self.lget(X_T1);
                self.i(W::I64Eqz);
                self.set(L_FLAG + 3);
                self.lget(X_T1);
                self.k(0);
                self.i(W::I64Ne);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                if instr.mnemonic() == Bsf {
                    self.lget(X_T1);
                    self.i(W::I64Ctz);
                } else {
                    self.k(63);
                    self.lget(X_T1);
                    self.i(W::I64Clz);
                    self.i(W::I64Sub);
                }
                self.lset(X_T2);
                self.rset_from(r, X_T2);
                self.depth -= 1;
                self.i(W::End);
            }
            // 64-bit code gets the instructions themselves (code built for a CPU that has
            // them; the 32-bit side runs them as bsf/bsr, as CPUs before them did).
            Tzcnt | Lzcnt | Popcnt => {
                let w = width(instr, 0);
                let r = instr.op0_register();
                self.rop(instr, 1);
                self.lset(X_T1);
                self.materialize(flags::ALL);
                match instr.mnemonic() {
                    Tzcnt => {
                        self.lget(X_T1);
                        self.i(W::I64Ctz);
                        self.k(w as u64);
                        self.lget(X_T1);
                        self.k(0);
                        self.i(W::I64Ne);
                        self.i(W::Select);
                    }
                    Lzcnt => {
                        self.lget(X_T1);
                        self.i(W::I64Clz);
                        self.k(64 - w as u64);
                        self.i(W::I64Sub);
                    }
                    _ => {
                        self.lget(X_T1);
                        self.i(W::I64Popcnt);
                    }
                }
                self.lset(X_T2);
                self.rset_from(r, X_T2);
                if instr.mnemonic() == Popcnt {
                    // ZF = source zero, the others clear
                    self.lget(X_T1);
                    self.i(W::I64Eqz);
                    self.set(L_FLAG + 3);
                    for fl in [0u32, 1, 2, 4, 5] {
                        self.c(0);
                        self.set(L_FLAG + fl);
                    }
                } else {
                    // CF = source zero, ZF = result zero
                    self.lget(X_T1);
                    self.i(W::I64Eqz);
                    self.set(L_FLAG);
                    self.lget(X_T2);
                    self.i(W::I64Eqz);
                    self.set(L_FLAG + 3);
                }
            }
            Bswap => {
                let r = instr.op0_register();
                if r.size() == 8 {
                    self.lget(gpr(r));
                    self.i(W::I64x2Splat);
                    self.i(W::V128Const(0));
                    self.i(W::I8x16Shuffle([7, 6, 5, 4, 3, 2, 1, 0, 16, 16, 16, 16, 16, 16, 16, 16]));
                    self.i(W::I64x2ExtractLane(0));
                    self.lset(gpr(r));
                } else {
                    // (bswap r16 is undefined: as the 32-bit side)
                    self.lget(gpr(r));
                    self.i(W::I64x2Splat);
                    self.i(W::V128Const(0));
                    self.i(W::I8x16Shuffle([3, 2, 1, 0, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16]));
                    self.i(W::I64x2ExtractLane(0));
                    self.lset(X_T1);
                    self.rset_from(r, X_T1);
                }
            }
            Xadd => {
                let w = width(instr, 0);
                let d = self.prep(instr, 0);
                self.rdst(d, w);
                self.lset(X_FA);
                self.rop(instr, 1);
                self.lset(X_FB);
                self.lget(X_FA);
                self.lget(X_FB);
                self.i(W::I64Add);
                self.mask_to(w);
                self.lset(X_FR);
                self.rset_from(instr.op1_register(), X_FA);
                self.wdst(d, w, X_FR);
                self.set_pending(FlagOp::Add, w);
            }
            Cmpxchg => {
                let w = width(instr, 0);
                let a = acc(w);
                let d = self.prep(instr, 0);
                self.rget(a);
                self.lset(X_FA);
                self.rdst(d, w);
                self.lset(X_FB);
                self.lget(X_FA);
                self.lget(X_FB);
                self.i(W::I64Sub);
                self.mask_to(w);
                self.lset(X_FR);
                self.set_pending(FlagOp::Sub, w);
                self.lget(X_FA);
                self.lget(X_FB);
                self.i(W::I64Eq);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.rop(instr, 1);
                self.lset(X_T1);
                self.wdst(d, w, X_T1);
                self.i(W::Else);
                // the accumulator gets the destination; a register destination is left
                // alone (writing it would zero-extend a 32-bit one), a memory one is
                // unchanged, so neither is written
                self.rset_from(a, X_FB);
                self.depth -= 1;
                self.i(W::End);
            }
            Cmpxchg8b | Cmpxchg16b => {
                let wide = instr.mnemonic() == Cmpxchg16b;
                let (lo_r, hi_r, nlo, nhi) = if wide {
                    (Register::RAX, Register::RDX, Register::RBX, Register::RCX)
                } else {
                    (Register::EAX, Register::EDX, Register::EBX, Register::ECX)
                };
                let half = if wide { 8 } else { 4 };
                self.addr_x64(instr);
                self.set(L_T0);
                self.get(L_T0);
                self.load64(half);
                self.lset(X_T1);
                self.get(L_T0);
                self.c(half as u32);
                self.i(W::I32Add);
                self.load64(half);
                self.lset(X_T2);
                self.rget(lo_r);
                self.lget(X_T1);
                self.i(W::I64Eq);
                self.rget(hi_r);
                self.lget(X_T2);
                self.i(W::I64Eq);
                self.i(W::I32And);
                self.set(L_T1);
                self.materialize(flags::ALL);
                self.get(L_T1);
                self.set(L_FLAG + 3);
                self.get(L_T1);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                for (r, off) in [(nlo, 0), (nhi, half as u32)] {
                    self.get(L_T0);
                    if off != 0 {
                        self.c(off);
                        self.i(W::I32Add);
                    }
                    self.rget(r);
                    self.store64(half);
                }
                self.i(W::Else);
                self.rset_from(lo_r, X_T1);
                self.rset_from(hi_r, X_T2);
                self.depth -= 1;
                self.i(W::End);
            }
            Lahf => {
                self.materialize(flags::ALL);
                // AH = SF:ZF:0:AF:0:PF:1:CF
                self.get(L_FLAG + 4);
                self.c(7);
                self.i(W::I32Shl);
                self.get(L_FLAG + 3);
                self.c(6);
                self.i(W::I32Shl);
                self.i(W::I32Or);
                self.get(L_FLAG + 2);
                self.c(4);
                self.i(W::I32Shl);
                self.i(W::I32Or);
                self.get(L_FLAG + 1);
                self.c(2);
                self.i(W::I32Shl);
                self.i(W::I32Or);
                self.get(L_FLAG);
                self.i(W::I32Or);
                self.c(2);
                self.i(W::I32Or);
                self.set(L_T1);
                self.write_reg_from(Register::AH, L_T1);
            }
            Sahf => {
                self.materialize(flags::ALL);
                self.read_reg(Register::AH);
                self.set(L_T1);
                for (bit, fl) in [(0u32, 0u32), (2, 1), (4, 2), (6, 3), (7, 4)] {
                    self.get(L_T1);
                    self.c(bit);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.set(L_FLAG + fl);
                }
            }
            Pushfq | Pushf => {
                self.eflags_to(L_T1);
                self.get(L_T1);
                self.i(W::I64ExtendI32U);
                self.lset(X_T1);
                if instr.mnemonic() == Pushf {
                    self.lget(RSP);
                    self.k(2);
                    self.i(W::I64Sub);
                    self.ltee(RSP);
                    self.i(W::I32WrapI64);
                    self.lget(X_T1);
                    self.i(W::I64Store16(mem0()));
                } else {
                    self.push64_from(X_T1);
                }
            }
            Popfq | Popf => {
                if instr.mnemonic() == Popf {
                    self.lget(RSP);
                    self.i(W::I32WrapI64);
                    self.i(W::I32Load16U(mem0()));
                    self.set(L_T1);
                    self.lget(RSP);
                    self.k(2);
                    self.i(W::I64Add);
                    self.lset(RSP);
                } else {
                    self.pop64_lo_to(L_T1);
                }
                self.eflags_from(L_T1);
            }
            Clc => self.set_flag_const(flags::CF, 0),
            Stc => self.set_flag_const(flags::CF, 1),
            Cmc => {
                self.materialize(flags::ALL);
                self.get(L_FLAG);
                self.c(1);
                self.i(W::I32Xor);
                self.set(L_FLAG);
            }
            Cld => {
                self.c(0);
                self.i(W::GlobalSet(Global::Df as u32));
            }
            Std => {
                self.c(1);
                self.i(W::GlobalSet(Global::Df as u32));
            }
            // The runtime's cpuid and rdtsc work on the 32-bit globals; the results
            // zero-extend.
            Cpuid | Rdtsc => {
                if instr.mnemonic() == Cpuid {
                    for (r, g) in [(RAX, Global::Eax), (RCX, Global::Ecx)] {
                        self.lget(r);
                        self.i(W::I32WrapI64);
                        self.i(W::GlobalSet(g as u32));
                    }
                    self.call_rt(RtFunc::Cpuid);
                } else {
                    self.call_rt(RtFunc::Rdtsc);
                }
                let out: &[(u32, Global)] = if instr.mnemonic() == Cpuid {
                    &[(RAX, Global::Eax), (RCX, Global::Ecx), (RDX, Global::Edx), (RBX, Global::Ebx)]
                } else {
                    &[(RAX, Global::Eax), (RDX, Global::Edx)]
                };
                for &(r, g) in out {
                    self.i(W::GlobalGet(g as u32));
                    self.i(W::I64ExtendI32U);
                    self.lset(r);
                }
            }
            Movsb | Movsw | Movsd | Movsq if matches!(instr.op0_kind(), OpKind::MemoryESRDI | OpKind::MemoryESEDI) => self.string_op64(instr),
            Stosb | Stosw | Stosd | Stosq | Lodsb | Lodsw | Lodsd | Lodsq | Scasb | Scasw | Scasd | Scasq | Cmpsb | Cmpsw | Cmpsq => {
                self.string_op64(instr)
            }
            Cmpsd if matches!(instr.op0_kind(), OpKind::MemorySegRSI | OpKind::MemorySegESI) => self.string_op64(instr),
            Xlatb => {
                self.lget(RBX);
                if instr.memory_base() == Register::EBX {
                    self.i(W::I32WrapI64);
                    self.i(W::I64ExtendI32U);
                }
                self.rget(Register::AL);
                self.i(W::I64Add);
                self.i(W::I32WrapI64);
                self.load64(1);
                self.lset(X_T1);
                self.rset_from(Register::AL, X_T1);
            }
            Int3 | Int | Ud2 | Hlt | Int1 => {
                // Only reached when not a block terminator (cannot happen); treat as trap.
                self.trap(trap::UNIMPLEMENTED, self.ip);
            }
            // x87 in 64-bit code (setjmp's control word, the odd long double routine):
            // the 32-bit path's helper, out of line
            m if crate::cfg::is_x87(m) || matches!(m, Emms | Femms | Wait) => {
                let inline = std::mem::replace(&mut self.x87_inline, false);
                self.x87(instr);
                self.x87_inline = inline;
            }
            _ => {
                if !self.sse(instr) {
                    self.unimplemented(instr);
                }
            }
        }
    }

    fn alu64(&mut self, instr: &Instruction, op: FlagOp, wop: W<'static>, writes: bool) {
        let w = width(instr, 0);
        if matches!(op, FlagOp::Adc | FlagOp::Sbb) {
            // the carry is read before the pending op's operands are replaced
            self.flag(flags::CF);
            self.set(L_FC);
            self.pending = None;
        }
        let d = self.prep(instr, 0);
        self.rdst(d, w);
        self.lset(X_FA);
        self.rop(instr, 1);
        self.lset(X_FB);
        self.lget(X_FA);
        self.lget(X_FB);
        self.i(wop);
        if matches!(op, FlagOp::Adc | FlagOp::Sbb) {
            self.get(L_FC);
            self.i(W::I64ExtendI32U);
            self.i(if op == FlagOp::Adc { W::I64Add } else { W::I64Sub });
        }
        self.mask_to(w);
        self.lset(X_FR);
        if writes {
            self.wdst(d, w, X_FR);
        }
        self.set_pending(op, w);
    }

    /// Pushes the shift/rotate count (i64) of a count operand: immediate, CL or 1,
    /// masked as x86 does (to 6 bits for 64-bit operands, else 5).
    fn count64(&mut self, instr: &Instruction, n: u32, w: u8) -> Option<u64> {
        let cm: u64 = if w == 64 { 63 } else { 31 };
        match instr.op_kind(n) {
            OpKind::Immediate8 => Some(instr.immediate8() as u64 & cm),
            OpKind::Register => {
                self.rget(Register::CL);
                self.k(cm);
                self.i(W::I64And);
                None
            }
            _ => Some(1),
        }
    }

    fn shift64(&mut self, instr: &Instruction) {
        let w = width(instr, 0);
        let d = self.prep(instr, 0);
        let op = match instr.mnemonic() {
            Mnemonic::Sar => FlagOp::Sar,
            Mnemonic::Shr => FlagOp::Shr,
            _ => FlagOp::Shl,
        };
        let wop = match op {
            FlagOp::Sar => W::I64ShrS,
            FlagOp::Shr => W::I64ShrU,
            _ => W::I64Shl,
        };
        let cm: u64 = if w == 64 { 63 } else { 31 };
        let stat = match instr.op1_kind() {
            OpKind::Immediate8 => Some(instr.immediate8() as u64 & cm),
            OpKind::Register => None,
            _ => Some(1),
        };
        // The pending flag op owns X_FA/X_FB/X_FR until it is replaced or materialized:
        // a count of zero must not touch them, and a run-time count materializes first.
        match stat {
            Some(0) => self.rewrite32(d, w),
            Some(n) => {
                self.rdst(d, w);
                self.lset(X_FA);
                self.k(n);
                self.lset(X_FB);
                self.lget(X_FA);
                if op == FlagOp::Sar {
                    self.sext64(w);
                }
                self.k(n);
                self.i(wop);
                self.mask_to(w);
                self.lset(X_FR);
                self.wdst(d, w, X_FR);
                self.set_pending(op, w);
            }
            None => {
                // a zero count leaves the flags: decided at run time, computing only
                // those read later
                let needed = self.needed;
                self.materialize(needed);
                self.rdst(d, w);
                self.lset(X_FA);
                self.rget(Register::CL);
                self.k(cm);
                self.i(W::I64And);
                self.ltee(X_FB);
                self.k(0);
                self.i(W::I64Ne);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.lget(X_FA);
                if op == FlagOp::Sar {
                    self.sext64(w);
                }
                self.lget(X_FB);
                self.i(wop);
                self.mask_to(w);
                self.lset(X_FR);
                self.wdst(d, w, X_FR);
                self.pending = Some((op, w));
                self.materialize(needed);
                self.i(W::Else);
                self.rewrite32_from(d, w, X_FA);
                self.depth -= 1;
                self.i(W::End);
                self.pending = None;
            }
        }
    }

    /// A 32-bit register destination of an operation that leaves its value (a count of
    /// zero) is still written: its upper half clears.
    fn rewrite32(&mut self, d: D, w: u8) {
        if let D::Reg(r) = d
            && w == 32
        {
            self.lget(gpr(r));
            self.i(W::I32WrapI64);
            self.i(W::I64ExtendI32U);
            self.lset(gpr(r));
        }
    }

    fn rewrite32_from(&mut self, d: D, w: u8, src: u32) {
        if let D::Reg(r) = d
            && w == 32
        {
            self.rset_from(r, src);
        }
    }

    fn rotate64(&mut self, instr: &Instruction) {
        let w = width(instr, 0);
        let wq = w as u64;
        let d = self.prep(instr, 0);
        self.materialize(flags::ALL);
        self.rdst(d, w);
        self.lset(X_FA);
        if let Some(n) = self.count64(instr, 1, w) {
            self.k(n);
        }
        self.lset(X_FB);
        self.lget(X_FB);
        self.k(0);
        self.i(W::I64Ne);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        let msb_of = |me: &mut Self, l: u32, bit: u64| {
            me.lget(l);
            me.k(bit);
            me.i(W::I64ShrU);
            me.i(W::I32WrapI64);
            me.c(1);
            me.i(W::I32And);
        };
        match instr.mnemonic() {
            Mnemonic::Rol | Mnemonic::Ror => {
                let left = instr.mnemonic() == Mnemonic::Rol;
                self.lget(X_FB);
                self.k(wq - 1);
                self.i(W::I64And);
                self.lset(X_T1);
                if w == 64 {
                    self.lget(X_FA);
                    self.lget(X_T1);
                    self.i(if left { W::I64Rotl } else { W::I64Rotr });
                } else {
                    // (a << n) | (a >> (w - n)) within the width
                    self.lget(X_FA);
                    self.lget(X_T1);
                    self.i(if left { W::I64Shl } else { W::I64ShrU });
                    self.lget(X_FA);
                    self.k(wq);
                    self.lget(X_T1);
                    self.i(W::I64Sub);
                    self.k(63);
                    self.i(W::I64And);
                    self.i(if left { W::I64ShrU } else { W::I64Shl });
                    self.i(W::I64Or);
                    self.mask_to(w);
                }
                self.lset(X_FR);
                self.wdst(d, w, X_FR);
                // CF = the bit rotated into the low (rol) or high (ror) position
                if left {
                    msb_of(self, X_FR, 0);
                } else {
                    msb_of(self, X_FR, wq - 1);
                }
                self.set(L_FLAG);
                // OF (count 1): msb(res) ^ CF for rol; msb(res) ^ bit w-2 of res for ror
                msb_of(self, X_FR, wq - 1);
                if left {
                    self.get(L_FLAG);
                } else {
                    msb_of(self, X_FR, wq - 2);
                }
                self.i(W::I32Xor);
                self.set(L_FLAG + 5);
            }
            _ => {
                // rcl/rcr: through the carry, count mod (w + 1), a bit at a time (rare)
                let left = instr.mnemonic() == Mnemonic::Rcl;
                self.lget(X_FB);
                self.k(wq + 1);
                self.i(W::I64RemU);
                self.ltee(X_T1);
                self.k(0);
                self.i(W::I64Ne);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.lget(X_FA);
                self.lset(X_FR);
                self.i(W::Block(BlockType::Empty));
                self.i(W::Loop(BlockType::Empty));
                self.lget(X_T1);
                self.i(W::I64Eqz);
                self.i(W::BrIf(1));
                if left {
                    // new cf = msb(res); res = (res << 1 | cf) & mask
                    msb_of(self, X_FR, wq - 1);
                    self.set(L_T2);
                    self.lget(X_FR);
                    self.k(1);
                    self.i(W::I64Shl);
                    self.get(L_FLAG);
                    self.i(W::I64ExtendI32U);
                    self.i(W::I64Or);
                    self.mask_to(w);
                    self.lset(X_FR);
                } else {
                    msb_of(self, X_FR, 0);
                    self.set(L_T2);
                    self.lget(X_FR);
                    self.k(1);
                    self.i(W::I64ShrU);
                    self.get(L_FLAG);
                    self.i(W::I64ExtendI32U);
                    self.k(wq - 1);
                    self.i(W::I64Shl);
                    self.i(W::I64Or);
                    self.lset(X_FR);
                }
                self.get(L_T2);
                self.set(L_FLAG);
                self.lget(X_T1);
                self.k(1);
                self.i(W::I64Sub);
                self.lset(X_T1);
                self.i(W::Br(0));
                self.i(W::End);
                self.i(W::End);
                self.wdst(d, w, X_FR);
                // OF (count 1): msb(res) ^ cf (rcl) / msb(res) ^ bit w-2 of res (rcr)
                msb_of(self, X_FR, wq - 1);
                if left {
                    self.get(L_FLAG);
                } else {
                    msb_of(self, X_FR, wq - 2);
                }
                self.i(W::I32Xor);
                self.set(L_FLAG + 5);
                self.i(W::Else);
                self.rewrite32_from(d, w, X_FA);
                self.depth -= 1;
                self.i(W::End);
            }
        }
        self.i(W::Else);
        self.rewrite32_from(d, w, X_FA);
        self.depth -= 1;
        self.i(W::End);
    }

    fn double_shift64(&mut self, instr: &Instruction) {
        let w = width(instr, 0);
        let wq = w as u64;
        let left = instr.mnemonic() == Mnemonic::Shld;
        let d = self.prep(instr, 0);
        self.materialize(flags::ALL);
        self.rdst(d, w);
        self.lset(X_FA);
        self.rop(instr, 1);
        self.lset(X_T1);
        if let Some(n) = self.count64(instr, 2, w) {
            self.k(n);
        }
        self.lset(X_FB);
        self.lget(X_FB);
        self.k(0);
        self.i(W::I64Ne);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        if w == 64 {
            // shld: dst << n | src >> (64 - n); shrd: dst >> n | src << (64 - n)
            self.lget(X_FA);
            self.lget(X_FB);
            self.i(if left { W::I64Shl } else { W::I64ShrU });
            self.lget(X_T1);
            self.k(64);
            self.lget(X_FB);
            self.i(W::I64Sub);
            self.i(if left { W::I64ShrU } else { W::I64Shl });
            self.i(W::I64Or);
        } else {
            // the two concatenated in 64 bits: shld hi = dst, lo = src; shrd the other way
            let (hi, lo) = if left { (X_FA, X_T1) } else { (X_T1, X_FA) };
            self.lget(hi);
            self.k(wq);
            self.i(W::I64Shl);
            self.lget(lo);
            self.i(W::I64Or);
            self.lget(X_FB);
            if left {
                self.i(W::I64Shl);
                self.k(wq);
                self.i(W::I64ShrU);
            } else {
                self.i(W::I64ShrU);
            }
            self.mask_to(w);
        }
        self.lset(X_FR);
        self.wdst(d, w, X_FR);
        // CF: the last bit shifted out
        self.lget(X_FA);
        if left {
            self.k(wq);
            self.lget(X_FB);
            self.i(W::I64Sub);
        } else {
            self.lget(X_FB);
            self.k(1);
            self.i(W::I64Sub);
        }
        self.i(W::I64ShrU);
        self.i(W::I32WrapI64);
        self.c(1);
        self.i(W::I32And);
        self.set(L_FLAG);
        // ZF/SF/PF from the result, OF = the sign changed (count 1), AF undefined
        self.lget(X_FR);
        self.i(W::I64Eqz);
        self.set(L_FLAG + 3);
        self.lget(X_FR);
        self.k(wq - 1);
        self.i(W::I64ShrU);
        self.i(W::I32WrapI64);
        self.c(1);
        self.i(W::I32And);
        self.set(L_FLAG + 4);
        self.lget(X_FR);
        self.i(W::I32WrapI64);
        self.c(0xff);
        self.i(W::I32And);
        self.i(W::I32Popcnt);
        self.c(1);
        self.i(W::I32And);
        self.c(1);
        self.i(W::I32Xor);
        self.set(L_FLAG + 1);
        self.lget(X_FR);
        self.lget(X_FA);
        self.i(W::I64Xor);
        self.k(wq - 1);
        self.i(W::I64ShrU);
        self.i(W::I32WrapI64);
        self.c(1);
        self.i(W::I32And);
        self.set(L_FLAG + 5);
        self.i(W::Else);
        self.rewrite32_from(d, w, X_FA);
        self.depth -= 1;
        self.i(W::End);
    }

    /// X_T2 = the high 64 bits of the 128-bit product X_T0 * X_T1 (unsigned, or
    /// signed); X_T3..X_T6 are scratch.
    fn mul_high64(&mut self, signed: bool) {
        let m = 0xffff_ffffu64;
        let half = |me: &mut Self, l: u32, high: bool| {
            me.lget(l);
            if high {
                me.k(32);
                me.i(W::I64ShrU);
            } else {
                me.k(m);
                me.i(W::I64And);
            }
        };
        for (dst, ah, bh) in [(X_T3, false, false), (X_T4, false, true), (X_T5, true, false), (X_T6, true, true)] {
            half(self, X_T0, ah);
            half(self, X_T1, bh);
            self.i(W::I64Mul);
            self.lset(dst);
        }
        // mid = (p00 >> 32) + (p01 & m) + (p10 & m)
        half(self, X_T3, true);
        half(self, X_T4, false);
        self.i(W::I64Add);
        half(self, X_T5, false);
        self.i(W::I64Add);
        self.lset(X_T3);
        // hi = p11 + (p01 >> 32) + (p10 >> 32) + (mid >> 32)
        self.lget(X_T6);
        half(self, X_T4, true);
        self.i(W::I64Add);
        half(self, X_T5, true);
        self.i(W::I64Add);
        half(self, X_T3, true);
        self.i(W::I64Add);
        if signed {
            // as signed: less b when a < 0, less a when b < 0
            for (x, y) in [(X_T1, X_T0), (X_T0, X_T1)] {
                self.lget(x);
                self.k(0);
                self.lget(y);
                self.k(0);
                self.i(W::I64LtS);
                self.i(W::Select);
                self.i(W::I64Sub);
            }
        }
        self.lset(X_T2);
    }

    fn imul64(&mut self, instr: &Instruction) {
        let w = width(instr, 0);
        self.materialize(flags::ALL);
        if instr.op_count() == 1 {
            if w == 64 {
                self.rop(instr, 0);
                self.lset(X_T1);
                self.lget(RAX);
                self.lset(X_T0);
                self.mul_high64(true);
                self.lget(X_T0);
                self.lget(X_T1);
                self.i(W::I64Mul);
                self.lset(RAX);
                self.lget(X_T2);
                self.lset(RDX);
                // CF = OF = the high half is not the sign of the low one
                self.lget(RDX);
                self.lget(RAX);
                self.k(63);
                self.i(W::I64ShrS);
                self.i(W::I64Ne);
                self.tee(L_FLAG);
                self.set(L_FLAG + 5);
            } else {
                self.rop(instr, 0);
                self.sext64(w);
                self.rget(acc(w));
                self.sext64(w);
                self.i(W::I64Mul);
                self.lset(X_T0);
                self.wide_result64(w, true);
            }
            return;
        }
        let r = instr.op0_register();
        self.rop(instr, 1);
        self.sext64(w);
        self.lset(X_T0);
        if instr.op_count() == 3 {
            self.rop(instr, 2);
        } else {
            self.rget(r);
        }
        self.sext64(w);
        self.lset(X_T1);
        // CF = OF = the product does not fit. For the 64-bit form the high half comes
        // first (`mul_high64` keeps X_T0/X_T1 but scribbles over X_T2..X_T6), then the
        // low product in X_T5.
        let flags_needed = self.needed & (flags::CF | flags::OF) != 0;
        if w == 64 && flags_needed {
            self.mul_high64(true);
            self.lget(X_T2);
            self.lset(X_FR);
        }
        self.lget(X_T0);
        self.lget(X_T1);
        self.i(W::I64Mul);
        self.lset(X_T5);
        if w == 64 {
            if flags_needed {
                self.lget(X_FR);
                self.lget(X_T5);
                self.k(63);
                self.i(W::I64ShrS);
                self.i(W::I64Ne);
                self.tee(L_FLAG);
                self.set(L_FLAG + 5);
            }
        } else {
            self.lget(X_T5);
            self.lget(X_T5);
            self.sext64(w);
            self.i(W::I64Ne);
            self.tee(L_FLAG);
            self.set(L_FLAG + 5);
        }
        self.rset_from(r, X_T5);
    }

    fn mul64(&mut self, instr: &Instruction) {
        let w = width(instr, 0);
        self.materialize(flags::ALL);
        if w == 64 {
            self.rop(instr, 0);
            self.lset(X_T1);
            self.lget(RAX);
            self.lset(X_T0);
            self.mul_high64(false);
            self.lget(X_T0);
            self.lget(X_T1);
            self.i(W::I64Mul);
            self.lset(RAX);
            self.lget(X_T2);
            self.lset(RDX);
            self.lget(RDX);
            self.k(0);
            self.i(W::I64Ne);
            self.tee(L_FLAG);
            self.set(L_FLAG + 5);
            return;
        }
        self.rop(instr, 0);
        self.rget(acc(w));
        self.i(W::I64Mul);
        self.lset(X_T0);
        self.wide_result64(w, false);
    }

    /// Writes the 2w-bit product (w <= 32) in X_T0 to its destination and sets CF/OF.
    fn wide_result64(&mut self, w: u8, signed: bool) {
        match w {
            8 => self.rset_from(Register::AX, X_T0),
            16 => {
                self.rset_from(Register::AX, X_T0);
                self.lget(X_T0);
                self.k(16);
                self.i(W::I64ShrU);
                self.lset(X_T1);
                self.rset_from(Register::DX, X_T1);
            }
            _ => {
                self.rset_from(Register::EAX, X_T0);
                self.lget(X_T0);
                self.k(32);
                self.i(W::I64ShrU);
                self.lset(X_T1);
                self.rset_from(Register::EDX, X_T1);
            }
        }
        // CF = OF = the upper half is not the extension of the lower
        self.lget(X_T0);
        if !signed {
            self.k(mask64(2 * w));
            self.i(W::I64And);
        }
        self.lget(X_T0);
        if signed {
            self.sext64(w);
        } else {
            self.mask_to(w);
        }
        self.i(W::I64Ne);
        self.tee(L_FLAG);
        self.set(L_FLAG + 5);
    }

    /// `if (cond) #DE` for the i32 condition on the stack.
    fn divide_error_if(&mut self) {
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        self.trap(trap::DIVIDE_ERROR, self.ip);
        self.depth -= 1;
        self.i(W::End);
    }

    fn div64(&mut self, instr: &Instruction) {
        let w = width(instr, 0);
        let signed = instr.mnemonic() == Mnemonic::Idiv;
        self.materialize(flags::ALL);
        if w == 64 {
            return self.div128(instr, signed);
        }
        // dividend (2w bits, sign-extended for idiv) in X_T0, divisor in X_T1
        match w {
            8 => {
                self.rget(Register::AX);
                if signed {
                    self.sext64(16);
                }
            }
            16 => {
                self.rget(Register::DX);
                self.k(16);
                self.i(W::I64Shl);
                self.rget(Register::AX);
                self.i(W::I64Or);
                if signed {
                    self.sext64(32);
                }
            }
            _ => {
                self.rget(Register::EDX);
                self.k(32);
                self.i(W::I64Shl);
                self.rget(Register::EAX);
                self.i(W::I64Or);
            }
        }
        self.lset(X_T0);
        self.rop(instr, 0);
        if signed {
            self.sext64(w);
        }
        self.lset(X_T1);
        // #DE on a zero divisor, i64::MIN / -1 (wasm would trap) or a quotient overflow
        self.lget(X_T1);
        self.i(W::I64Eqz);
        if signed {
            self.lget(X_T0);
            self.k(i64::MIN as u64);
            self.i(W::I64Eq);
            self.lget(X_T1);
            self.k(u64::MAX);
            self.i(W::I64Eq);
            self.i(W::I32And);
            self.i(W::I32Or);
        }
        self.divide_error_if();
        self.lget(X_T0);
        self.lget(X_T1);
        self.i(if signed { W::I64DivS } else { W::I64DivU });
        self.lset(X_T2);
        self.lget(X_T0);
        self.lget(X_T1);
        self.i(if signed { W::I64RemS } else { W::I64RemU });
        self.lset(X_T3);
        self.lget(X_T2);
        if signed {
            self.k((-(1i64 << (w - 1))) as u64);
            self.i(W::I64LtS);
            self.lget(X_T2);
            self.k((1u64 << (w - 1)) - 1);
            self.i(W::I64GtS);
            self.i(W::I32Or);
        } else {
            self.k(mask64(w));
            self.i(W::I64GtU);
        }
        self.divide_error_if();
        let (q, r) = match w {
            8 => (Register::AL, Register::AH),
            16 => (Register::AX, Register::DX),
            _ => (Register::EAX, Register::EDX),
        };
        self.rset_from(q, X_T2);
        self.rset_from(r, X_T3);
    }

    /// div/idiv r/m64: rdx:rax by a 64-bit divisor. A dividend that fits 64 bits (rdx
    /// its extension, the common case) divides directly, others a bit at a time.
    fn div128(&mut self, instr: &Instruction, signed: bool) {
        self.rop(instr, 0);
        self.lset(X_T1);
        self.lget(X_T1);
        self.i(W::I64Eqz);
        self.divide_error_if();
        // fits: rdx == 0 (div) or rdx == rax >> 63 (idiv)
        self.lget(RDX);
        if signed {
            self.lget(RAX);
            self.k(63);
            self.i(W::I64ShrS);
        } else {
            self.k(0);
        }
        self.i(W::I64Eq);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        if signed {
            // (i64::MIN / -1 traps in wasm)
            self.lget(RAX);
            self.k(i64::MIN as u64);
            self.i(W::I64Eq);
            self.lget(X_T1);
            self.k(u64::MAX);
            self.i(W::I64Eq);
            self.i(W::I32And);
            self.divide_error_if();
        }
        self.lget(RAX);
        self.lget(X_T1);
        self.i(if signed { W::I64RemS } else { W::I64RemU });
        self.lget(RAX);
        self.lget(X_T1);
        self.i(if signed { W::I64DivS } else { W::I64DivU });
        self.lset(RAX);
        self.lset(RDX);
        self.i(W::Else);
        if signed {
            // X_T0: bit 0 the dividend is negative, bit 1 the divisor
            self.lget(RDX);
            self.k(63);
            self.i(W::I64ShrU);
            self.lget(X_T1);
            self.k(63);
            self.i(W::I64ShrU);
            self.k(1);
            self.i(W::I64Shl);
            self.i(W::I64Or);
            self.lset(X_T0);
            // |dividend| in X_T2:X_T3: a negative one negated, (~hi + (lo == 0), -lo)
            self.lget(RDX);
            self.k(u64::MAX);
            self.i(W::I64Xor);
            self.lget(RAX);
            self.i(W::I64Eqz);
            self.i(W::I64ExtendI32U);
            self.i(W::I64Add);
            self.lget(RDX);
            self.sign_bit(1);
            self.i(W::Select);
            self.lset(X_T2);
            self.k(0);
            self.lget(RAX);
            self.i(W::I64Sub);
            self.lget(RAX);
            self.sign_bit(1);
            self.i(W::Select);
            self.lset(X_T3);
            // |divisor| in X_T1
            self.k(0);
            self.lget(X_T1);
            self.i(W::I64Sub);
            self.lget(X_T1);
            self.sign_bit(2);
            self.i(W::Select);
            self.lset(X_T1);
        } else {
            self.lget(RDX);
            self.lset(X_T2);
            self.lget(RAX);
            self.lset(X_T3);
        }
        // the quotient must fit: high half below the divisor
        self.lget(X_T2);
        self.lget(X_T1);
        self.i(W::I64GeU);
        self.divide_error_if();
        self.udiv128();
        // quotient in X_T4, remainder in X_T2
        if signed {
            // X_T5: the quotient is negative (the signs differ); it fits up to 2^63 - 1,
            // or 2^63 when negative
            self.lget(X_T0);
            self.lget(X_T0);
            self.k(1);
            self.i(W::I64ShrU);
            self.i(W::I64Xor);
            self.k(1);
            self.i(W::I64And);
            self.lset(X_T5);
            self.lget(X_T4);
            self.k(i64::MAX as u64);
            self.lget(X_T5);
            self.i(W::I64Add);
            self.i(W::I64GtU);
            self.divide_error_if();
            // quotient with its sign, remainder with the dividend's
            self.k(0);
            self.lget(X_T4);
            self.i(W::I64Sub);
            self.lget(X_T4);
            self.lget(X_T5);
            self.i(W::I32WrapI64);
            self.i(W::Select);
            self.lset(RAX);
            self.k(0);
            self.lget(X_T2);
            self.i(W::I64Sub);
            self.lget(X_T2);
            self.sign_bit(1);
            self.i(W::Select);
            self.lset(RDX);
        } else {
            self.lget(X_T4);
            self.lset(RAX);
            self.lget(X_T2);
            self.lset(RDX);
        }
        self.depth -= 1;
        self.i(W::End);
    }

    /// Pushes X_T0 & `bit` as an i32 condition (div128's signs).
    fn sign_bit(&mut self, bit: u64) {
        self.lget(X_T0);
        self.k(bit);
        self.i(W::I64And);
        self.i(W::I32WrapI64);
    }

    /// X_T2:X_T3 (high < X_T1) divided by X_T1, restoring a bit at a time: quotient to
    /// X_T4, remainder to X_T2. X_T6 and L_T2 are scratch.
    fn udiv128(&mut self) {
        self.k(0);
        self.lset(X_T4);
        self.c(64);
        self.set(L_T2);
        self.i(W::Block(BlockType::Empty));
        self.i(W::Loop(BlockType::Empty));
        // carry out of the high half's shift
        self.lget(X_T2);
        self.k(63);
        self.i(W::I64ShrU);
        self.lset(X_T6);
        self.lget(X_T2);
        self.k(1);
        self.i(W::I64Shl);
        self.lget(X_T3);
        self.k(63);
        self.i(W::I64ShrU);
        self.i(W::I64Or);
        self.lset(X_T2);
        self.lget(X_T3);
        self.k(1);
        self.i(W::I64Shl);
        self.lset(X_T3);
        self.lget(X_T4);
        self.k(1);
        self.i(W::I64Shl);
        self.lset(X_T4);
        self.lget(X_T6);
        self.i(W::I64Eqz);
        self.i(W::I32Eqz);
        self.lget(X_T2);
        self.lget(X_T1);
        self.i(W::I64GeU);
        self.i(W::I32Or);
        self.i(W::If(BlockType::Empty));
        self.lget(X_T2);
        self.lget(X_T1);
        self.i(W::I64Sub);
        self.lset(X_T2);
        self.lget(X_T4);
        self.k(1);
        self.i(W::I64Or);
        self.lset(X_T4);
        self.i(W::End);
        self.get(L_T2);
        self.c(1);
        self.i(W::I32Sub);
        self.tee(L_T2);
        self.i(W::BrIf(0));
        self.i(W::End);
        self.i(W::End);
    }

    fn bit_test64(&mut self, instr: &Instruction) {
        let w = width(instr, 0);
        self.materialize(flags::ALL);
        // the bit offset
        match instr.op1_kind() {
            OpKind::Immediate8 => self.k(instr.immediate8() as u64),
            _ => {
                self.rop(instr, 1);
                self.sext64(w);
            }
        }
        self.lset(X_T1);
        let d = match instr.op0_kind() {
            OpKind::Memory if instr.op1_kind() == OpKind::Register => {
                // a register offset addresses memory beyond the operand (signed):
                // address += (offset >> log2(w)) * bytes
                self.addr_x64(instr);
                self.lget(X_T1);
                self.k((w as u64).trailing_zeros() as u64);
                self.i(W::I64ShrS);
                self.k((w as u64 / 8).trailing_zeros() as u64);
                self.i(W::I64Shl);
                self.i(W::I32WrapI64);
                self.i(W::I32Add);
                self.set(L_T0);
                D::Mem
            }
            _ => self.prep(instr, 0),
        };
        self.lget(X_T1);
        self.k(w as u64 - 1);
        self.i(W::I64And);
        self.lset(X_T1);
        self.rdst(d, w);
        self.lset(X_T2);
        self.lget(X_T2);
        self.lget(X_T1);
        self.i(W::I64ShrU);
        self.i(W::I32WrapI64);
        self.c(1);
        self.i(W::I32And);
        self.set(L_FLAG);
        let modify = match instr.mnemonic() {
            Mnemonic::Bts => Some(0),
            Mnemonic::Btr => Some(1),
            Mnemonic::Btc => Some(2),
            _ => None,
        };
        if let Some(m) = modify {
            self.lget(X_T2);
            self.k(1);
            self.lget(X_T1);
            self.i(W::I64Shl);
            match m {
                0 => self.i(W::I64Or),
                1 => {
                    self.k(u64::MAX);
                    self.i(W::I64Xor);
                    self.i(W::I64And);
                }
                _ => self.i(W::I64Xor),
            }
            self.mask_to(w);
            self.lset(X_T2);
            self.wdst(d, w, X_T2);
        }
    }

    fn string_op64(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        let m = instr.mnemonic();
        let w: u8 = match m {
            Movsb | Stosb | Lodsb | Scasb | Cmpsb => 8,
            Movsw | Stosw | Lodsw | Scasw | Cmpsw => 16,
            Movsq | Stosq | Lodsq | Scasq | Cmpsq => 64,
            _ => 32,
        };
        let bytes = w as usize / 8;
        let step = bytes as u64;
        // a 32-bit address size counts in ecx/esi/edi
        let a32 = (0..instr.op_count()).any(|n| matches!(instr.op_kind(n), OpKind::MemoryESEDI | OpKind::MemorySegESI));
        let rep = instr.has_rep_prefix() || instr.has_repe_prefix() || instr.has_repne_prefix();
        let compare = matches!(m, Scasb | Scasw | Scasd | Scasq | Cmpsb | Cmpsw | Cmpsd | Cmpsq);
        let gs = instr.segment_prefix() == Register::GS;
        if compare {
            self.materialize(flags::ALL);
        }
        // delta = df ? -step : step
        self.k(step.wrapping_neg());
        self.k(step);
        self.i(W::GlobalGet(Global::Df as u32));
        self.i(W::Select);
        self.lset(X_T3);
        if rep {
            self.i(W::Block(BlockType::Empty));
            self.i(W::Loop(BlockType::Empty));
            self.depth += 2;
            self.lget(RCX);
            if a32 {
                self.i(W::I32WrapI64);
                self.i(W::I32Eqz);
            } else {
                self.i(W::I64Eqz);
            }
            self.i(W::BrIf(1));
        }
        let at = |me: &mut Self, r: u32, src: bool| {
            me.lget(r);
            me.i(W::I32WrapI64);
            if src && gs {
                me.i(W::GlobalGet(Global::GsBase as u32));
                me.i(W::I32Add);
            }
        };
        let advance = |me: &mut Self, r: u32| {
            me.lget(r);
            me.lget(X_T3);
            me.i(W::I64Add);
            if a32 {
                me.i(W::I32WrapI64);
                me.i(W::I64ExtendI32U);
            }
            me.lset(r);
        };
        match m {
            Movsb | Movsw | Movsd | Movsq => {
                at(self, RDI, false);
                at(self, RSI, true);
                self.load64(bytes);
                self.store64(bytes);
                advance(self, RSI);
                advance(self, RDI);
            }
            Stosb | Stosw | Stosd | Stosq => {
                at(self, RDI, false);
                self.rget(acc(w));
                self.store64(bytes);
                advance(self, RDI);
            }
            Lodsb | Lodsw | Lodsd | Lodsq => {
                at(self, RSI, true);
                self.load64(bytes);
                self.lset(X_T1);
                self.rset_from(acc(w), X_T1);
                advance(self, RSI);
            }
            Scasb | Scasw | Scasd | Scasq => {
                self.rget(acc(w));
                self.lset(X_FA);
                at(self, RDI, false);
                self.load64(bytes);
                self.lset(X_FB);
                advance(self, RDI);
                self.cmp_flags64(w);
            }
            _ => {
                at(self, RSI, true);
                self.load64(bytes);
                self.lset(X_FA);
                at(self, RDI, false);
                self.load64(bytes);
                self.lset(X_FB);
                advance(self, RSI);
                advance(self, RDI);
                self.cmp_flags64(w);
            }
        }
        if rep {
            self.lget(RCX);
            self.k(1);
            self.i(W::I64Sub);
            if a32 {
                self.i(W::I32WrapI64);
                self.i(W::I64ExtendI32U);
            }
            self.lset(RCX);
            if compare {
                // repe stops when ZF == 0, repne when ZF == 1
                self.get(L_FLAG + 3);
                if instr.has_repne_prefix() {
                    self.i(W::BrIf(1));
                } else {
                    self.i(W::I32Eqz);
                    self.i(W::BrIf(1));
                }
            }
            self.i(W::Br(0));
            self.depth -= 2;
            self.i(W::End);
            self.i(W::End);
        }
    }
}

pub const _CHECK: (u32, u32) = (L_D0, L_REG);
pub const _MEMARG: fn() -> MemArg = mem0;
