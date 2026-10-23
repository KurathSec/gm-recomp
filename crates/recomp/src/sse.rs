//! SSE/SSE2 lifting onto wasm SIMD. XMM registers are v128 globals; scalar
//! double/single operations work on lane 0 and keep the other lanes, as the
//! hardware does. NaN payloads are not reproduced (wasm does not define them).
//!
//! MMX registers are the x87 registers' mantissas, as on the hardware: mmN is
//! physical register RN in the thread's x87 block (`fpu::block`). Packed MMX
//! operations run on a v128 holding the 64-bit value in both halves and keep the
//! low half; the ones whose lanes cross the halves (packs, high unpacks, pshufw,
//! pmovmskb) arrange their operands first.

use crate::abi::Global;
use crate::flags;
use crate::lift::{FuncEmit, L_D0, L_FLAG, L_MM, L_MQ, L_Q0, L_T0, L_T1, L_V0, L_V1, mem0};
use fpu::block as fb;
use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind, Register};
use wasm_encoder::{BlockType, Instruction as W, MemArg};

fn xmm_global(r: Register) -> u32 {
    Global::Xmm0 as u32 + (r.number() as u32)
}

/// Shuffle indices for two v128 operands (a = lanes 0..15, b = 16..31).
fn shuffle_lanes(lane_bytes: u8, pick: impl Fn(u8) -> (bool, u8)) -> [u8; 16] {
    let mut out = [0u8; 16];
    let lanes = 16 / lane_bytes;
    for l in 0..lanes {
        let (from_b, src_lane) = pick(l);
        for k in 0..lane_bytes {
            out[(l * lane_bytes + k) as usize] = (if from_b { 16 } else { 0 }) + src_lane * lane_bytes + k;
        }
    }
    out
}

fn mem_at(offset: u32) -> MemArg {
    MemArg { offset: offset as u64, ..mem0() }
}

impl<'a> FuncEmit<'a> {
    /// Pushes an xmm register, or an mm register as an i64x2 splat.
    fn xget(&mut self, r: Register) {
        if r.is_mm() {
            self.mm_get(r);
            self.i(W::I64x2Splat);
        } else {
            self.i(W::GlobalGet(xmm_global(r)));
        }
    }

    /// Pops a v128 into an xmm register, or its low 64 bits into an mm register.
    fn xset(&mut self, r: Register) {
        if r.is_mm() {
            self.i(W::I64x2ExtractLane(0));
            self.mm_set(r);
        } else {
            self.i(W::GlobalSet(xmm_global(r)));
        }
    }

    /// L_MM = the thread's x87 block.
    fn mm_block(&mut self) {
        self.i(W::GlobalGet(Global::FsBase as u32));
        self.c(fb::OFFSET);
        self.i(W::I32Sub);
        self.i(W::LocalSet(L_MM));
    }

    /// Pushes mmN (i64): the 80-bit image's mantissa, or that of the double the
    /// register holds (a zero, an infinity or a normal number).
    fn mm_get(&mut self, r: Register) {
        let n = r.number() as u32;
        self.mm_block();
        self.i(W::LocalGet(L_MM));
        self.i(W::I64Load(mem_at(fb::EXT + 10 * n)));
        // from the double: exponent 0 -> 0, else (fraction | 1 << 52) << 11
        self.i(W::LocalGet(L_MM));
        self.i(W::I64Load(mem_at(fb::REGS + 8 * n)));
        self.i(W::LocalSet(L_MQ));
        self.i(W::LocalGet(L_MQ));
        self.i(W::I64Const((1 << 52) - 1));
        self.i(W::I64And);
        self.i(W::I64Const(1 << 52));
        self.i(W::I64Or);
        self.i(W::I64Const(11));
        self.i(W::I64Shl);
        self.i(W::I64Const(0));
        self.i(W::LocalGet(L_MQ));
        self.i(W::I64Const(0x7ff0_0000_0000_0000));
        self.i(W::I64And);
        self.i(W::I64Const(0));
        self.i(W::I64Ne);
        self.i(W::Select);
        self.i(W::LocalGet(L_MM));
        self.i(W::I32Load8U(mem_at(fb::KIND + n)));
        self.c(fb::EXT_IMAGE as u32);
        self.i(W::I32And);
        self.i(W::Select);
    }

    /// Pops an i64 into mmN: exponent and sign all ones, the register valid, TOP 0.
    fn mm_set(&mut self, r: Register) {
        let n = r.number() as u32;
        self.i(W::LocalSet(L_MQ));
        self.mm_block();
        self.i(W::LocalGet(L_MM));
        self.i(W::LocalGet(L_MQ));
        self.i(W::I64Store(mem_at(fb::EXT + 10 * n)));
        self.i(W::LocalGet(L_MM));
        self.c(0xFFFF);
        self.i(W::I32Store16(mem_at(fb::EXT + 10 * n + 8)));
        self.i(W::LocalGet(L_MM));
        self.c((fb::VALID | fb::EXT_IMAGE) as u32);
        self.i(W::I32Store8(mem_at(fb::KIND + n)));
        self.i(W::LocalGet(L_MM));
        self.c(0);
        self.i(W::I32Store(mem_at(fb::TOP)));
    }

    /// Pushes a v128 from an xmm/mm register or a memory operand (an 8-byte one is
    /// loaded with the upper half zero).
    fn vsrc(&mut self, instr: &Instruction, n: u32) {
        match instr.op_kind(n) {
            OpKind::Register => self.xget(instr.op_register(n)),
            OpKind::Memory => {
                self.addr(instr);
                if instr.memory_size().size() == 8 {
                    self.i(W::V128Load64Zero(mem0()));
                    if (0..instr.op_count()).any(|k| instr.op_kind(k) == OpKind::Register && instr.op_register(k).is_mm()) {
                        self.i(W::I64x2ExtractLane(0));
                        self.i(W::I64x2Splat);
                    }
                } else {
                    self.i(W::V128Load(mem0()));
                }
            }
            _ => unreachable!(),
        }
    }

    /// Pushes the low 64 bits (i64) of an xmm/mm register or an m64.
    fn src64(&mut self, instr: &Instruction, n: u32) {
        match instr.op_kind(n) {
            OpKind::Register if instr.op_register(n).is_mm() => self.mm_get(instr.op_register(n)),
            OpKind::Register => {
                self.xget(instr.op_register(n));
                self.i(W::I64x2ExtractLane(0));
            }
            OpKind::Memory => {
                self.addr(instr);
                self.i(W::I64Load(mem0()));
            }
            _ => unreachable!(),
        }
    }

    /// Pushes the low 32 bits (i32) of an xmm register or an m32.
    fn src32(&mut self, instr: &Instruction, n: u32) {
        match instr.op_kind(n) {
            OpKind::Register => {
                self.xget(instr.op_register(n));
                self.i(W::I32x4ExtractLane(0));
            }
            OpKind::Memory => {
                self.addr(instr);
                self.i(W::I32Load(mem0()));
            }
            _ => unreachable!(),
        }
    }

    fn src_f64(&mut self, instr: &Instruction, n: u32) {
        self.src64(instr, n);
        self.i(W::F64ReinterpretI64);
    }

    fn src_f32(&mut self, instr: &Instruction, n: u32) {
        self.src32(instr, n);
        self.i(W::F32ReinterpretI32);
    }

    /// Stores a v128 (on the stack) to the destination operand.
    fn vdst(&mut self, instr: &Instruction) {
        match instr.op0_kind() {
            OpKind::Register => self.xset(instr.op0_register()),
            OpKind::Memory => {
                self.i(W::LocalSet(L_V0));
                self.addr(instr);
                self.i(W::LocalGet(L_V0));
                self.i(W::V128Store(mem0()));
            }
            _ => unreachable!(),
        }
    }

    /// dst.lane0 (f64) = f(dst.lane0, src); other lanes unchanged.
    fn scalar_f64(&mut self, instr: &Instruction, op: W<'static>) {
        let d = instr.op0_register();
        self.xget(d);
        self.xget(d);
        self.i(W::F64x2ExtractLane(0));
        self.src_f64(instr, 1);
        self.i(op);
        self.i(W::F64x2ReplaceLane(0));
        self.xset(d);
    }

    fn scalar_f32(&mut self, instr: &Instruction, op: W<'static>) {
        let d = instr.op0_register();
        self.xget(d);
        self.xget(d);
        self.i(W::F32x4ExtractLane(0));
        self.src_f32(instr, 1);
        self.i(op);
        self.i(W::F32x4ReplaceLane(0));
        self.xset(d);
    }

    /// x86 min/max: `a < b ? a : b` (or `>`), which returns the source on NaN or equal zeros.
    fn scalar_minmax_f64(&mut self, instr: &Instruction, is_min: bool) {
        let d = instr.op0_register();
        self.xget(d);
        self.xget(d);
        self.i(W::F64x2ExtractLane(0));
        self.i(W::LocalSet(L_D0));
        self.src_f64(instr, 1);
        self.i(W::I64ReinterpretF64);
        self.i(W::LocalSet(L_Q0));
        // select(a, b, a < b)
        self.i(W::LocalGet(L_D0));
        self.i(W::LocalGet(L_Q0));
        self.i(W::F64ReinterpretI64);
        self.i(W::LocalGet(L_D0));
        self.i(W::LocalGet(L_Q0));
        self.i(W::F64ReinterpretI64);
        self.i(if is_min { W::F64Lt } else { W::F64Gt });
        self.i(W::Select);
        self.i(W::F64x2ReplaceLane(0));
        self.xset(d);
    }

    fn packed_minmax(&mut self, instr: &Instruction, is_min: bool, double: bool) {
        // r = bitselect(a, b, a < b)
        let d = instr.op0_register();
        self.xget(d);
        self.i(W::LocalSet(L_V0));
        self.vsrc(instr, 1);
        self.i(W::LocalSet(L_V1));
        self.i(W::LocalGet(L_V0));
        self.i(W::LocalGet(L_V1));
        self.i(W::LocalGet(L_V0));
        self.i(W::LocalGet(L_V1));
        self.i(match (is_min, double) {
            (true, true) => W::F64x2Lt,
            (false, true) => W::F64x2Gt,
            (true, false) => W::F32x4Lt,
            (false, false) => W::F32x4Gt,
        });
        self.i(W::V128Bitselect);
        self.xset(d);
    }

    /// comiss/ucomiss/comisd/ucomisd: ZF PF CF from the comparison, OF SF AF cleared.
    fn compare_flags(&mut self, instr: &Instruction, double: bool) {
        self.materialize(flags::ALL);
        let d = instr.op0_register();
        if double {
            self.xget(d);
            self.i(W::F64x2ExtractLane(0));
            self.i(W::LocalSet(L_D0));
            self.src_f64(instr, 1);
            self.i(W::I64ReinterpretF64);
            self.i(W::LocalSet(L_Q0));
            // unordered?
            self.i(W::LocalGet(L_D0));
            self.i(W::LocalGet(L_D0));
            self.i(W::F64Ne);
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::F64Ne);
            self.i(W::I32Or);
            self.i(W::LocalSet(L_T0));
            // ZF = unordered | eq ; PF = unordered ; CF = unordered | lt
            self.i(W::LocalGet(L_T0));
            self.i(W::LocalGet(L_D0));
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::F64Eq);
            self.i(W::I32Or);
            self.i(W::LocalSet(L_FLAG + 3));
            self.i(W::LocalGet(L_T0));
            self.i(W::LocalSet(L_FLAG + 1));
            self.i(W::LocalGet(L_T0));
            self.i(W::LocalGet(L_D0));
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::F64Lt);
            self.i(W::I32Or);
            self.i(W::LocalSet(L_FLAG));
        } else {
            self.xget(d);
            self.i(W::F32x4ExtractLane(0));
            self.i(W::F64PromoteF32);
            self.i(W::LocalSet(L_D0));
            self.src_f32(instr, 1);
            self.i(W::F64PromoteF32);
            self.i(W::I64ReinterpretF64);
            self.i(W::LocalSet(L_Q0));
            self.i(W::LocalGet(L_D0));
            self.i(W::LocalGet(L_D0));
            self.i(W::F64Ne);
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::F64Ne);
            self.i(W::I32Or);
            self.i(W::LocalSet(L_T0));
            self.i(W::LocalGet(L_T0));
            self.i(W::LocalGet(L_D0));
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::F64Eq);
            self.i(W::I32Or);
            self.i(W::LocalSet(L_FLAG + 3));
            self.i(W::LocalGet(L_T0));
            self.i(W::LocalSet(L_FLAG + 1));
            self.i(W::LocalGet(L_T0));
            self.i(W::LocalGet(L_D0));
            self.i(W::LocalGet(L_Q0));
            self.i(W::F64ReinterpretI64);
            self.i(W::F64Lt);
            self.i(W::I32Or);
            self.i(W::LocalSet(L_FLAG));
        }
        for fl in [2u32, 4, 5] {
            self.c(0);
            self.i(W::LocalSet(L_FLAG + fl));
        }
    }

    /// f64 -> i32 with x86 semantics: NaN or out of range gives 0x80000000.
    fn f64_to_i32(&mut self, truncate: bool) {
        // value on stack (f64) -> local d0
        self.i(W::LocalSet(L_D0));
        if !truncate {
            self.i(W::LocalGet(L_D0));
            self.i(W::F64Nearest);
            self.i(W::LocalSet(L_D0));
        }
        // in range iff -2^31 <= v < 2^31 (NaN fails both)
        self.i(W::LocalGet(L_D0));
        self.i(W::I32TruncSatF64S);
        self.c(0x8000_0000);
        self.i(W::LocalGet(L_D0));
        self.i(W::F64Const((-2147483648.0f64).into()));
        self.i(W::F64Ge);
        self.i(W::LocalGet(L_D0));
        self.i(W::F64Const((2147483648.0f64).into()));
        self.i(W::F64Lt);
        self.i(W::I32And);
        self.i(W::Select);
    }

    /// Shift count handling: x86 zeroes the result when the count exceeds the lane width
    /// (arithmetic shifts saturate to the sign); wasm masks the count instead.
    fn packed_shift(&mut self, instr: &Instruction, lane_bits: u32, op: W<'static>, arith: bool) {
        let d = instr.op0_register();
        // count -> t0
        match instr.op1_kind() {
            OpKind::Immediate8 => self.c(instr.immediate8() as u32),
            _ => {
                // count is the low 64 bits of an xmm/m64; anything >= 64 saturates
                self.src64(instr, 1);
                self.i(W::LocalSet(L_Q0));
                self.i(W::LocalGet(L_Q0));
                self.i(W::I32WrapI64);
                self.c(255);
                self.i(W::LocalGet(L_Q0));
                self.i(W::I64Const(255));
                self.i(W::I64LtU);
                self.i(W::Select);
            }
        }
        self.i(W::LocalSet(L_T0));
        self.xget(d);
        self.i(W::LocalSet(L_V0));
        // if count >= lane_bits: result = arith ? (x >> (lane_bits-1)) : 0
        self.i(W::LocalGet(L_T0));
        self.c(lane_bits);
        self.i(W::I32GeU);
        self.i(W::If(BlockType::Empty));
        self.i(W::LocalGet(L_V0));
        if arith {
            self.c(lane_bits - 1);
            self.i(op.clone());
        } else {
            self.i(W::V128Const(0));
            self.i(W::V128And);
        }
        self.i(W::LocalSet(L_V0));
        self.i(W::Else);
        self.i(W::LocalGet(L_V0));
        self.i(W::LocalGet(L_T0));
        self.i(op);
        self.i(W::LocalSet(L_V0));
        self.i(W::End);
        self.i(W::LocalGet(L_V0));
        self.xset(d);
    }

    /// Returns false when the instruction is not an SSE form this module handles.
    pub(crate) fn sse(&mut self, instr: &Instruction) -> bool {
        use Mnemonic::*;
        let m = instr.mnemonic();
        let reg0 = instr.op0_kind() == OpKind::Register;
        let reg1 = instr.op_count() > 1 && instr.op1_kind() == OpKind::Register;
        let is_xmm0 = reg0 && instr.op0_register().is_xmm();
        let is_xmm1 = reg1 && instr.op1_register().is_xmm();
        let is_mm0 = reg0 && instr.op0_register().is_mm();
        let is_mm1 = reg1 && instr.op1_register().is_mm();
        match m {
            // ---- MMX moves and the forms that differ from the xmm ones ---------------------
            Movq | Movntq if is_mm0 || is_mm1 => {
                if is_mm0 {
                    self.src64(instr, 1);
                    self.mm_set(instr.op0_register());
                } else {
                    self.addr(instr);
                    self.mm_get(instr.op1_register());
                    self.i(W::I64Store(mem0()));
                }
            }
            Movd if is_mm0 || is_mm1 => {
                if is_mm0 {
                    self.read_op(instr, 1, false);
                    self.i(W::I64ExtendI32U);
                    self.mm_set(instr.op0_register());
                } else if instr.op0_kind() == OpKind::Memory {
                    self.addr(instr);
                    self.mm_get(instr.op1_register());
                    self.i(W::I32WrapI64);
                    self.i(W::I32Store(mem0()));
                } else {
                    self.mm_get(instr.op1_register());
                    self.i(W::I32WrapI64);
                    self.i(W::LocalSet(L_T1));
                    self.write_reg_from(instr.op0_register(), L_T1);
                }
            }
            Movq2dq => {
                self.mm_get(instr.op1_register());
                self.i(W::I64x2Splat);
                self.i(W::I64Const(0));
                self.i(W::I64x2ReplaceLane(1));
                self.xset(instr.op0_register());
            }
            Movdq2q => {
                self.xget(instr.op1_register());
                self.i(W::I64x2ExtractLane(0));
                self.mm_set(instr.op0_register());
            }
            Pmovmskb if is_mm1 => {
                self.xget(instr.op1_register());
                self.i(W::I8x16Bitmask);
                self.c(0xFF);
                self.i(W::I32And);
                self.i(W::LocalSet(L_T1));
                self.write_reg_from(instr.op0_register(), L_T1);
            }
            Pextrw if is_mm1 => {
                self.xget(instr.op1_register());
                self.i(W::I16x8ExtractLaneU(instr.immediate8() & 3));
                self.i(W::LocalSet(L_T1));
                self.write_reg_from(instr.op0_register(), L_T1);
            }
            Pinsrw if is_mm0 => {
                let d = instr.op0_register();
                self.xget(d);
                self.read_op(instr, 1, false);
                self.i(W::I16x8ReplaceLane(instr.immediate8() & 3));
                self.xset(d);
            }
            Pshufw => {
                let imm = instr.immediate8();
                self.vsrc(instr, 1);
                self.i(W::LocalSet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::I8x16Shuffle(shuffle_lanes(2, |l| (false, (imm >> (2 * (l % 4))) & 3))));
                self.xset(instr.op0_register());
            }
            Packuswb | Packsswb | Packssdw if is_mm0 => {
                // narrow the i64x2 (dst, src): its low half is the result
                let d = instr.op0_register();
                self.mm_get(d);
                self.i(W::I64x2Splat);
                self.src64(instr, 1);
                self.i(W::I64x2ReplaceLane(1));
                self.i(W::LocalSet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(match m { Packuswb => W::I8x16NarrowI16x8U, Packsswb => W::I8x16NarrowI16x8S, _ => W::I16x8NarrowI32x4S });
                self.xset(d);
            }
            Cvtpi2ps => {
                // two i32 -> the low two floats; the upper two stay
                let d = instr.op0_register();
                self.xget(d);
                self.src64(instr, 1);
                self.i(W::I64x2Splat);
                self.i(W::F32x4ConvertI32x4S);
                self.i(W::I8x16Shuffle(shuffle_lanes(4, |l| if l < 2 { (true, l) } else { (false, l) })));
                self.xset(d);
            }
            Cvtpi2pd => {
                self.src64(instr, 1);
                self.i(W::I64x2Splat);
                self.i(W::F64x2ConvertLowI32x4S);
                self.xset(instr.op0_register());
            }
            // ---- moves -----------------------------------------------------------------
            Movaps | Movups | Movapd | Movupd | Movdqa | Movdqu | Movntdq | Movntps | Movntpd | Lddqu => {
                self.vsrc(instr, 1);
                self.vdst(instr);
            }
            Movsd if is_xmm0 || is_xmm1 => {
                if is_xmm0 && is_xmm1 {
                    // merge low lane
                    let d = instr.op0_register();
                    self.xget(d);
                    self.xget(instr.op1_register());
                    self.i(W::I64x2ExtractLane(0));
                    self.i(W::I64x2ReplaceLane(0));
                    self.xset(d);
                } else if is_xmm0 {
                    // load: zero upper
                    self.src64(instr, 1);
                    self.i(W::I64x2Splat);
                    self.i(W::I64Const(0));
                    self.i(W::I64x2ReplaceLane(1));
                    self.xset(instr.op0_register());
                } else {
                    self.addr(instr);
                    self.xget(instr.op1_register());
                    self.i(W::I64x2ExtractLane(0));
                    self.i(W::I64Store(mem0()));
                }
            }
            Movss => {
                if is_xmm0 && is_xmm1 {
                    let d = instr.op0_register();
                    self.xget(d);
                    self.xget(instr.op1_register());
                    self.i(W::I32x4ExtractLane(0));
                    self.i(W::I32x4ReplaceLane(0));
                    self.xset(d);
                } else if is_xmm0 {
                    self.src32(instr, 1);
                    self.i(W::I32x4Splat);
                    self.i(W::V128Const(0));
                    self.i(W::I8x16Shuffle([0, 1, 2, 3, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27]));
                    self.xset(instr.op0_register());
                } else {
                    self.addr(instr);
                    self.xget(instr.op1_register());
                    self.i(W::I32x4ExtractLane(0));
                    self.i(W::I32Store(mem0()));
                }
            }
            Movq if is_xmm0 || is_xmm1 => {
                if is_xmm0 {
                    // xmm <- xmm/m64, upper zeroed
                    self.src64(instr, 1);
                    self.i(W::I64x2Splat);
                    self.i(W::I64Const(0));
                    self.i(W::I64x2ReplaceLane(1));
                    self.xset(instr.op0_register());
                } else if instr.op0_kind() == OpKind::Memory {
                    self.addr(instr);
                    self.xget(instr.op1_register());
                    self.i(W::I64x2ExtractLane(0));
                    self.i(W::I64Store(mem0()));
                } else {
                    return false; // mm forms
                }
            }
            Movd if is_xmm0 || is_xmm1 => {
                if is_xmm0 {
                    // xmm <- r/m32, zero extended
                    self.read_op(instr, 1, false);
                    self.i(W::I32x4Splat);
                    self.i(W::V128Const(0));
                    self.i(W::I8x16Shuffle([0, 1, 2, 3, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27]));
                    self.xset(instr.op0_register());
                } else {
                    // r/m32 <- xmm low
                    let dst_mem = instr.op0_kind() == OpKind::Memory;
                    if dst_mem {
                        self.addr(instr);
                    }
                    self.xget(instr.op1_register());
                    self.i(W::I32x4ExtractLane(0));
                    if dst_mem {
                        self.i(W::I32Store(mem0()));
                    } else {
                        self.i(W::LocalSet(L_T1));
                        let r = instr.op0_register();
                        self.write_reg_from(r, L_T1);
                    }
                }
            }
            Movlps | Movlpd => {
                if is_xmm0 {
                    // xmm.low <- m64
                    let d = instr.op0_register();
                    self.xget(d);
                    self.src64(instr, 1);
                    self.i(W::I64x2ReplaceLane(0));
                    self.xset(d);
                } else {
                    self.addr(instr);
                    self.xget(instr.op1_register());
                    self.i(W::I64x2ExtractLane(0));
                    self.i(W::I64Store(mem0()));
                }
            }
            Movhps | Movhpd => {
                if is_xmm0 {
                    let d = instr.op0_register();
                    self.xget(d);
                    self.src64(instr, 1);
                    self.i(W::I64x2ReplaceLane(1));
                    self.xset(d);
                } else {
                    self.addr(instr);
                    self.xget(instr.op1_register());
                    self.i(W::I64x2ExtractLane(1));
                    self.i(W::I64Store(mem0()));
                }
            }
            Movhlps => {
                let d = instr.op0_register();
                self.xget(d);
                self.xget(instr.op1_register());
                self.i(W::I64x2ExtractLane(1));
                self.i(W::I64x2ReplaceLane(0));
                self.xset(d);
            }
            Movlhps => {
                let d = instr.op0_register();
                self.xget(d);
                self.xget(instr.op1_register());
                self.i(W::I64x2ExtractLane(0));
                self.i(W::I64x2ReplaceLane(1));
                self.xset(d);
            }
            Movmskps | Movmskpd | Pmovmskb if is_xmm1 => {
                self.xget(instr.op1_register());
                self.i(match m { Movmskps => W::I32x4Bitmask, Movmskpd => W::I64x2Bitmask, _ => W::I8x16Bitmask });
                self.i(W::LocalSet(L_T1));
                let r = instr.op0_register();
                self.write_reg_from(r, L_T1);
            }
            Pextrw if is_xmm1 => {
                self.xget(instr.op1_register());
                self.i(W::I16x8ExtractLaneU(instr.immediate8() & 7));
                self.i(W::LocalSet(L_T1));
                let r = instr.op0_register();
                self.write_reg_from(r, L_T1);
            }
            Pinsrw if is_xmm0 => {
                let d = instr.op0_register();
                self.xget(d);
                self.read_op(instr, 1, false);
                self.i(W::I16x8ReplaceLane(instr.immediate8() & 7));
                self.xset(d);
            }
            // ---- scalar arithmetic ------------------------------------------------------
            Addsd => self.scalar_f64(instr, W::F64Add),
            Subsd => self.scalar_f64(instr, W::F64Sub),
            Mulsd => self.scalar_f64(instr, W::F64Mul),
            Divsd => self.scalar_f64(instr, W::F64Div),
            Addss => self.scalar_f32(instr, W::F32Add),
            Subss => self.scalar_f32(instr, W::F32Sub),
            Mulss => self.scalar_f32(instr, W::F32Mul),
            Divss => self.scalar_f32(instr, W::F32Div),
            Sqrtsd => {
                let d = instr.op0_register();
                self.xget(d);
                self.src_f64(instr, 1);
                self.i(W::F64Sqrt);
                self.i(W::F64x2ReplaceLane(0));
                self.xset(d);
            }
            Sqrtss => {
                let d = instr.op0_register();
                self.xget(d);
                self.src_f32(instr, 1);
                self.i(W::F32Sqrt);
                self.i(W::F32x4ReplaceLane(0));
                self.xset(d);
            }
            // Approximate reciprocals (FMOD's mixer): the exact value. Processors return
            // ~12-bit approximations that differ between makers.
            Rcpss | Rsqrtss => {
                let d = instr.op0_register();
                self.xget(d);
                self.i(W::F32Const(1.0f32.into()));
                self.src_f32(instr, 1);
                if m == Rsqrtss {
                    self.i(W::F32Sqrt);
                }
                self.i(W::F32Div);
                self.i(W::F32x4ReplaceLane(0));
                self.xset(d);
            }
            Rcpps | Rsqrtps => {
                let d = instr.op0_register();
                self.i(W::F32Const(1.0f32.into()));
                self.i(W::F32x4Splat);
                self.vsrc(instr, 1);
                if m == Rsqrtps {
                    self.i(W::F32x4Sqrt);
                }
                self.i(W::F32x4Div);
                self.xset(d);
            }
            Minsd => self.scalar_minmax_f64(instr, true),
            Maxsd => self.scalar_minmax_f64(instr, false),
            Minss | Maxss => {
                let d = instr.op0_register();
                self.xget(d);
                self.xget(d);
                self.i(W::F32x4ExtractLane(0));
                self.i(W::I32ReinterpretF32);
                self.i(W::LocalSet(L_T0));
                self.i(W::LocalGet(L_T0));
                self.i(W::F32ReinterpretI32);
                self.src_f32(instr, 1);
                self.i(W::I32ReinterpretF32);
                self.i(W::LocalSet(L_T1));
                self.i(W::LocalGet(L_T1));
                self.i(W::F32ReinterpretI32);
                self.i(W::LocalGet(L_T0));
                self.i(W::F32ReinterpretI32);
                self.i(W::LocalGet(L_T1));
                self.i(W::F32ReinterpretI32);
                self.i(if m == Minss { W::F32Lt } else { W::F32Gt });
                self.i(W::Select);
                self.i(W::F32x4ReplaceLane(0));
                self.xset(d);
            }
            // ---- packed float arithmetic ------------------------------------------------
            Addps | Subps | Mulps | Divps | Addpd | Subpd | Mulpd | Divpd | Sqrtps | Sqrtpd => {
                let d = instr.op0_register();
                if matches!(m, Sqrtps | Sqrtpd) {
                    self.vsrc(instr, 1);
                    self.i(if m == Sqrtps { W::F32x4Sqrt } else { W::F64x2Sqrt });
                } else {
                    self.xget(d);
                    self.vsrc(instr, 1);
                    self.i(match m {
                        Addps => W::F32x4Add,
                        Subps => W::F32x4Sub,
                        Mulps => W::F32x4Mul,
                        Divps => W::F32x4Div,
                        Addpd => W::F64x2Add,
                        Subpd => W::F64x2Sub,
                        Mulpd => W::F64x2Mul,
                        _ => W::F64x2Div,
                    });
                }
                self.xset(d);
            }
            Minps => self.packed_minmax(instr, true, false),
            Maxps => self.packed_minmax(instr, false, false),
            Minpd => self.packed_minmax(instr, true, true),
            Maxpd => self.packed_minmax(instr, false, true),
            Andps | Andpd | Pand | Orps | Orpd | Por | Xorps | Xorpd | Pxor | Andnps | Andnpd | Pandn => {
                let d = instr.op0_register();
                if matches!(m, Andnps | Andnpd | Pandn) {
                    // dst = src & ~dst
                    self.vsrc(instr, 1);
                    self.xget(d);
                    self.i(W::V128AndNot);
                } else {
                    self.xget(d);
                    self.vsrc(instr, 1);
                    self.i(match m { Andps | Andpd | Pand => W::V128And, Orps | Orpd | Por => W::V128Or, _ => W::V128Xor });
                }
                self.xset(d);
            }
            // ---- conversions --------------------------------------------------------------
            Cvtss2sd => {
                let d = instr.op0_register();
                self.xget(d);
                self.src_f32(instr, 1);
                self.i(W::F64PromoteF32);
                self.i(W::F64x2ReplaceLane(0));
                self.xset(d);
            }
            Cvtsd2ss => {
                let d = instr.op0_register();
                self.xget(d);
                self.src_f64(instr, 1);
                self.i(W::F32DemoteF64);
                self.i(W::F32x4ReplaceLane(0));
                self.xset(d);
            }
            Cvtsi2sd => {
                let d = instr.op0_register();
                self.xget(d);
                self.read_op(instr, 1, false);
                self.i(W::F64ConvertI32S);
                self.i(W::F64x2ReplaceLane(0));
                self.xset(d);
            }
            Cvtsi2ss => {
                let d = instr.op0_register();
                self.xget(d);
                self.read_op(instr, 1, false);
                self.i(W::F32ConvertI32S);
                self.i(W::F32x4ReplaceLane(0));
                self.xset(d);
            }
            Cvtsd2si | Cvttsd2si => {
                self.src_f64(instr, 1);
                self.f64_to_i32(m == Cvttsd2si);
                self.i(W::LocalSet(L_T1));
                let r = instr.op0_register();
                self.write_reg_from(r, L_T1);
            }
            Cvtss2si | Cvttss2si => {
                self.src_f32(instr, 1);
                self.i(W::F64PromoteF32);
                self.f64_to_i32(m == Cvttss2si);
                self.i(W::LocalSet(L_T1));
                let r = instr.op0_register();
                self.write_reg_from(r, L_T1);
            }
            Cvtps2pd => {
                self.vsrc(instr, 1);
                self.i(W::F64x2PromoteLowF32x4);
                self.xset(instr.op0_register());
            }
            Cvtpd2ps => {
                self.vsrc(instr, 1);
                self.i(W::F32x4DemoteF64x2Zero);
                self.xset(instr.op0_register());
            }
            Cvtdq2pd => {
                self.vsrc(instr, 1);
                self.i(W::F64x2ConvertLowI32x4S);
                self.xset(instr.op0_register());
            }
            Cvtdq2ps => {
                self.vsrc(instr, 1);
                self.i(W::F32x4ConvertI32x4S);
                self.xset(instr.op0_register());
            }
            Cvttps2dq | Cvtps2dq | Cvttps2pi | Cvtps2pi => {
                // out-of-range and NaN lanes become 0x80000000 (the pi forms keep two)
                self.vsrc(instr, 1);
                if matches!(m, Cvtps2dq | Cvtps2pi) {
                    self.i(W::F32x4Nearest);
                }
                self.i(W::LocalSet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::I32x4TruncSatF32x4S);
                self.i(W::V128Const(0x8000_0000_8000_0000_8000_0000_8000_0000u128 as i128));
                // in range: v >= -2^31 && v < 2^31
                self.i(W::LocalGet(L_V0));
                self.i(W::F32Const((-2147483648.0f32).into()));
                self.i(W::F32x4Splat);
                self.i(W::F32x4Ge);
                self.i(W::LocalGet(L_V0));
                self.i(W::F32Const((2147483648.0f32).into()));
                self.i(W::F32x4Splat);
                self.i(W::F32x4Lt);
                self.i(W::V128And);
                self.i(W::V128Bitselect);
                self.xset(instr.op0_register());
            }
            Cvttpd2dq | Cvtpd2dq | Cvttpd2pi | Cvtpd2pi => {
                self.vsrc(instr, 1);
                if matches!(m, Cvtpd2dq | Cvtpd2pi) {
                    self.i(W::F64x2Nearest);
                }
                self.i(W::LocalSet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::I32x4TruncSatF64x2SZero);
                self.i(W::V128Const(0x8000_0000_8000_0000u128 as i128));
                self.i(W::LocalGet(L_V0));
                self.i(W::F64Const((-2147483648.0f64).into()));
                self.i(W::F64x2Splat);
                self.i(W::F64x2Ge);
                self.i(W::LocalGet(L_V0));
                self.i(W::F64Const((2147483648.0f64).into()));
                self.i(W::F64x2Splat);
                self.i(W::F64x2Lt);
                self.i(W::V128And);
                // the i64 masks cover lanes 0/1 of the i32 result; upper lanes stay zero
                self.i(W::V128Const(0));
                self.i(W::I8x16Shuffle([0, 1, 2, 3, 8, 9, 10, 11, 16, 17, 18, 19, 16, 17, 18, 19]));
                self.i(W::V128Bitselect);
                self.i(W::V128Const(0x0000_0000_0000_0000_ffff_ffff_ffff_ffffu128 as i128));
                self.i(W::V128And);
                self.xset(instr.op0_register());
            }
            // ---- compares -----------------------------------------------------------------
            Comiss | Ucomiss => self.compare_flags(instr, false),
            Comisd | Ucomisd => self.compare_flags(instr, true),
            Cmpps | Cmppd | Cmpss | Cmpsd if instr.op_count() == 3 => {
                let pred = instr.immediate8() & 7;
                let d = instr.op0_register();
                let double = matches!(m, Cmppd | Cmpsd);
                let scalar = matches!(m, Cmpss | Cmpsd);
                if scalar {
                    self.xget(d);
                    self.xget(d);
                    if double { self.i(W::F64x2ExtractLane(0)); self.i(W::LocalSet(L_D0)); self.src_f64(instr, 1); }
                    else { self.i(W::F32x4ExtractLane(0)); self.i(W::F64PromoteF32); self.i(W::LocalSet(L_D0)); self.src_f32(instr, 1); self.i(W::F64PromoteF32); }
                    self.i(W::I64ReinterpretF64);
                    self.i(W::LocalSet(L_Q0));
                    self.emit_pred_scalar(pred);
                    if double {
                        self.i(W::I64ExtendI32S);
                        self.i(W::I64x2ReplaceLane(0));
                    } else {
                        self.i(W::I32x4ReplaceLane(0));
                    }
                    self.xset(d);
                } else {
                    self.xget(d);
                    self.i(W::LocalSet(L_V0));
                    self.vsrc(instr, 1);
                    self.i(W::LocalSet(L_V1));
                    self.emit_pred_packed(pred, double);
                    self.xset(d);
                }
            }
            Pcmpeqb | Pcmpeqw | Pcmpeqd | Pcmpgtb | Pcmpgtw | Pcmpgtd if is_xmm0 || is_mm0 => {
                let d = instr.op0_register();
                self.xget(d);
                self.vsrc(instr, 1);
                self.i(match m {
                    Pcmpeqb => W::I8x16Eq,
                    Pcmpeqw => W::I16x8Eq,
                    Pcmpeqd => W::I32x4Eq,
                    Pcmpgtb => W::I8x16GtS,
                    Pcmpgtw => W::I16x8GtS,
                    _ => W::I32x4GtS,
                });
                self.xset(d);
            }
            // ---- packed integer arithmetic -------------------------------------------------
            Paddb | Paddw | Paddd | Paddq | Psubb | Psubw | Psubd | Psubq | Paddsb | Paddsw | Paddusb | Paddusw | Psubsb
            | Psubsw | Psubusb | Psubusw | Pmullw | Pmaddwd | Pmaxsw | Pminsw | Pmaxub | Pminub | Pavgb | Pavgw
                if is_xmm0 || is_mm0 =>
            {
                let d = instr.op0_register();
                self.xget(d);
                self.vsrc(instr, 1);
                self.i(match m {
                    Paddb => W::I8x16Add,
                    Paddw => W::I16x8Add,
                    Paddd => W::I32x4Add,
                    Paddq => W::I64x2Add,
                    Psubb => W::I8x16Sub,
                    Psubw => W::I16x8Sub,
                    Psubd => W::I32x4Sub,
                    Psubq => W::I64x2Sub,
                    Paddsb => W::I8x16AddSatS,
                    Paddsw => W::I16x8AddSatS,
                    Paddusb => W::I8x16AddSatU,
                    Paddusw => W::I16x8AddSatU,
                    Psubsb => W::I8x16SubSatS,
                    Psubsw => W::I16x8SubSatS,
                    Psubusb => W::I8x16SubSatU,
                    Psubusw => W::I16x8SubSatU,
                    Pmullw => W::I16x8Mul,
                    Pmaddwd => W::I32x4DotI16x8S,
                    Pmaxsw => W::I16x8MaxS,
                    Pminsw => W::I16x8MinS,
                    Pmaxub => W::I8x16MaxU,
                    Pminub => W::I8x16MinU,
                    Pavgb => W::I8x16AvgrU,
                    _ => W::I16x8AvgrU,
                });
                self.xset(d);
            }
            Pmulhw | Pmulhuw if is_xmm0 || is_mm0 => {
                // high halves of the 16x16 products
                let d = instr.op0_register();
                self.xget(d);
                self.i(W::LocalSet(L_V0));
                self.vsrc(instr, 1);
                self.i(W::LocalSet(L_V1));
                self.i(W::LocalGet(L_V0));
                self.i(W::LocalGet(L_V1));
                self.i(if m == Pmulhw { W::I32x4ExtMulLowI16x8S } else { W::I32x4ExtMulLowI16x8U });
                self.c(16);
                self.i(W::I32x4ShrS);
                self.i(W::LocalGet(L_V0));
                self.i(W::LocalGet(L_V1));
                self.i(if m == Pmulhw { W::I32x4ExtMulHighI16x8S } else { W::I32x4ExtMulHighI16x8U });
                self.c(16);
                self.i(W::I32x4ShrS);
                self.i(W::I16x8NarrowI32x4S);
                self.xset(d);
            }
            Pmuludq if is_xmm0 || is_mm0 => {
                // x86 multiplies lanes 0 and 2 (the low dwords of each qword); gather them first
                let d = instr.op0_register();
                self.xget(d);
                self.i(W::LocalSet(L_V0));
                self.vsrc(instr, 1);
                self.i(W::LocalSet(L_V1));
                self.i(W::LocalGet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::I8x16Shuffle([0, 1, 2, 3, 8, 9, 10, 11, 0, 1, 2, 3, 8, 9, 10, 11]));
                self.i(W::LocalGet(L_V1));
                self.i(W::LocalGet(L_V1));
                self.i(W::I8x16Shuffle([0, 1, 2, 3, 8, 9, 10, 11, 0, 1, 2, 3, 8, 9, 10, 11]));
                self.i(W::I64x2ExtMulLowI32x4U);
                self.xset(d);
            }
            Packuswb | Packsswb | Packssdw if is_xmm0 => {
                let d = instr.op0_register();
                self.xget(d);
                self.vsrc(instr, 1);
                self.i(match m { Packuswb => W::I8x16NarrowI16x8U, Packsswb => W::I8x16NarrowI16x8S, _ => W::I16x8NarrowI32x4S });
                self.xset(d);
            }
            // ---- shifts -----------------------------------------------------------------------
            Psllw if is_xmm0 || is_mm0 => self.packed_shift(instr, 16, W::I16x8Shl, false),
            Pslld if is_xmm0 || is_mm0 => self.packed_shift(instr, 32, W::I32x4Shl, false),
            Psllq if is_xmm0 || is_mm0 => self.packed_shift(instr, 64, W::I64x2Shl, false),
            Psrlw if is_xmm0 || is_mm0 => self.packed_shift(instr, 16, W::I16x8ShrU, false),
            Psrld if is_xmm0 || is_mm0 => self.packed_shift(instr, 32, W::I32x4ShrU, false),
            Psrlq if is_xmm0 || is_mm0 => self.packed_shift(instr, 64, W::I64x2ShrU, false),
            Psraw if is_xmm0 || is_mm0 => self.packed_shift(instr, 16, W::I16x8ShrS, true),
            Psrad if is_xmm0 || is_mm0 => self.packed_shift(instr, 32, W::I32x4ShrS, true),
            Pslldq | Psrldq => {
                let d = instr.op0_register();
                let n = instr.immediate8().min(16);
                let mut idx = [16u8; 16];
                for k in 0..16u8 {
                    idx[k as usize] = if m == Pslldq {
                        if k >= n { k - n } else { 16 }
                    } else if k + n < 16 { k + n } else { 16 };
                }
                self.xget(d);
                self.i(W::V128Const(0));
                self.i(W::I8x16Shuffle(idx));
                self.xset(d);
            }
            // ---- shuffles / unpacks ----------------------------------------------------------
            Pshufd | Pshuflw | Pshufhw => {
                let imm = instr.immediate8();
                let idx = match m {
                    Pshufd => shuffle_lanes(4, |l| (false, (imm >> (2 * l)) & 3)),
                    Pshuflw => shuffle_lanes(2, |l| (false, if l < 4 { (imm >> (2 * l)) & 3 } else { l })),
                    _ => shuffle_lanes(2, |l| (false, if l >= 4 { 4 + ((imm >> (2 * (l - 4))) & 3) } else { l })),
                };
                self.vsrc(instr, 1);
                self.i(W::LocalSet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::LocalGet(L_V0));
                self.i(W::I8x16Shuffle(idx));
                self.xset(instr.op0_register());
            }
            Shufps | Shufpd => {
                let imm = instr.immediate8();
                let idx = if m == Shufps {
                    shuffle_lanes(4, |l| if l < 2 { (false, (imm >> (2 * l)) & 3) } else { (true, (imm >> (2 * l)) & 3) })
                } else {
                    shuffle_lanes(8, |l| if l == 0 { (false, imm & 1) } else { (true, (imm >> 1) & 1) })
                };
                let d = instr.op0_register();
                self.xget(d);
                self.vsrc(instr, 1);
                self.i(W::I8x16Shuffle(idx));
                self.xset(d);
            }
            Unpcklps | Unpckhps | Unpcklpd | Unpckhpd | Punpcklbw | Punpcklwd | Punpckldq | Punpcklqdq | Punpckhbw
            | Punpckhwd | Punpckhdq | Punpckhqdq => {
                let (lane, high) = match m {
                    Unpcklps | Punpckldq => (4, false),
                    Unpckhps | Punpckhdq => (4, true),
                    Unpcklpd | Punpcklqdq => (8, false),
                    Unpckhpd | Punpckhqdq => (8, true),
                    Punpcklbw => (1, false),
                    Punpckhbw => (1, true),
                    Punpcklwd => (2, false),
                    _ => (2, true),
                };
                // within the 64 bits of an mm register, or the 128 of an xmm one
                let half = if is_mm0 { 8 } else { 16 } / lane / 2;
                let idx = shuffle_lanes(lane, |l| {
                    let src = l / 2 + if high { half } else { 0 };
                    (l % 2 == 1, src)
                });
                let d = instr.op0_register();
                self.xget(d);
                self.vsrc(instr, 1);
                self.i(W::I8x16Shuffle(idx));
                self.xset(d);
            }
            // ---- control -------------------------------------------------------------------
            Ldmxcsr => {
                self.addr(instr);
                self.i(W::I32Load(mem0()));
                self.i(W::GlobalSet(Global::Mxcsr as u32));
            }
            Stmxcsr => {
                self.addr(instr);
                self.i(W::GlobalGet(Global::Mxcsr as u32));
                self.i(W::I32Store(mem0()));
            }
            Movnti => {
                // movnti r32 -> m32
                self.addr(instr);
                self.read_op(instr, 1, false);
                self.i(W::I32Store(mem0()));
            }
            _ => return false,
        }
        true
    }

    /// Scalar compare predicate on d0 (f64) vs q0 (f64 bits) -> i32 mask (0 / -1).
    fn emit_pred_scalar(&mut self, pred: u8) {
        let (ordered_test, negate) = match pred {
            0 => (Some(W::F64Eq), false),
            1 => (Some(W::F64Lt), false),
            2 => (Some(W::F64Le), false),
            3 => (None, false), // unordered
            4 => (Some(W::F64Eq), true), // neq (true for unordered)
            5 => (Some(W::F64Lt), true), // nlt
            6 => (Some(W::F64Le), true), // nle
            _ => (None, true),           // ordered
        };
        match ordered_test {
            Some(op) => {
                self.i(W::LocalGet(L_D0));
                self.i(W::LocalGet(L_Q0));
                self.i(W::F64ReinterpretI64);
                self.i(op);
                if negate {
                    self.i(W::I32Eqz);
                }
            }
            None => {
                // unordered = a != a || b != b
                self.i(W::LocalGet(L_D0));
                self.i(W::LocalGet(L_D0));
                self.i(W::F64Ne);
                self.i(W::LocalGet(L_Q0));
                self.i(W::F64ReinterpretI64);
                self.i(W::LocalGet(L_Q0));
                self.i(W::F64ReinterpretI64);
                self.i(W::F64Ne);
                self.i(W::I32Or);
                if negate {
                    self.i(W::I32Eqz);
                }
            }
        }
        // 0/1 -> 0/-1
        self.i(W::I32Eqz);
        self.c(1);
        self.i(W::I32Sub);
    }

    /// Packed compare predicate on v0 vs v1 -> v128 mask.
    fn emit_pred_packed(&mut self, pred: u8, double: bool) {
        let get = |me: &mut Self| {
            me.i(W::LocalGet(L_V0));
            me.i(W::LocalGet(L_V1));
        };
        let unordered = |me: &mut Self| {
            me.i(W::LocalGet(L_V0));
            me.i(W::LocalGet(L_V0));
            me.i(if double { W::F64x2Ne } else { W::F32x4Ne });
            me.i(W::LocalGet(L_V1));
            me.i(W::LocalGet(L_V1));
            me.i(if double { W::F64x2Ne } else { W::F32x4Ne });
            me.i(W::V128Or);
        };
        match pred {
            0 => { get(self); self.i(if double { W::F64x2Eq } else { W::F32x4Eq }); }
            1 => { get(self); self.i(if double { W::F64x2Lt } else { W::F32x4Lt }); }
            2 => { get(self); self.i(if double { W::F64x2Le } else { W::F32x4Le }); }
            3 => unordered(self),
            4 => { get(self); self.i(if double { W::F64x2Ne } else { W::F32x4Ne }); }
            5 => { get(self); self.i(if double { W::F64x2Lt } else { W::F32x4Lt }); self.i(W::V128Not); }
            6 => { get(self); self.i(if double { W::F64x2Le } else { W::F32x4Le }); self.i(W::V128Not); }
            _ => { unordered(self); self.i(W::V128Not); }
        }
    }
}

pub const _MEMARG_CHECK: fn() -> MemArg = mem0;
pub const _SIZE_CHECK: MemorySize = MemorySize::Unknown;
