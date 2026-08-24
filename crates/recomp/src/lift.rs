//! Instruction semantics: decoded x86 -> wasm, one guest function at a time.
//!
//! Registers are cached in wasm locals for the duration of a function and
//! spilled to the shared globals around calls and at exits. Arithmetic flags
//! are kept lazily (see `flags.rs`) and only computed when something reads
//! them. Control flow inside a function is a `br_table` dispatcher over the
//! function's basic blocks.

use crate::abi::{self, Global, RtFunc, trap, types};
use crate::cfg::{Block, Cfg, Term};
use crate::flags::{self, FlagOp};
use iced_x86::{ConditionCode, Instruction, Mnemonic, OpKind, Register};
use std::borrow::Cow;
use std::collections::HashMap;
use wasm_encoder::{BlockType, Function, Instruction as W, MemArg, ValType};

// Local indices.
pub(crate) const L_REG: u32 = 0; // eax..edi = 0..7
pub(crate) const L_FLAG: u32 = 8; // cf,pf,af,zf,sf,of = 8..13
pub(crate) const L_BLK: u32 = 14;
pub(crate) const L_FA: u32 = 15;
pub(crate) const L_FB: u32 = 16;
pub(crate) const L_FR: u32 = 17;
pub(crate) const L_FC: u32 = 18;
pub(crate) const L_T0: u32 = 19;
pub(crate) const L_T1: u32 = 20;
pub(crate) const L_T2: u32 = 21;
pub(crate) const L_T3: u32 = 22;
pub(crate) const L_Q0: u32 = 23;
pub(crate) const L_Q1: u32 = 24;
pub(crate) const L_D0: u32 = 25;
pub(crate) const L_V0: u32 = 26;
pub(crate) const L_V1: u32 = 27;
pub const LOCALS: [(u32, ValType); 4] = [(23, ValType::I32), (2, ValType::I64), (1, ValType::F64), (2, ValType::V128)];

pub(crate) fn mem0() -> MemArg {
    MemArg { offset: 0, align: 0, memory_index: 0 }
}

pub(crate) fn mask(w: u8) -> u32 {
    if w == 32 { 0xffff_ffff } else { (1u32 << w) - 1 }
}

/// Where an instruction's destination lives, after its address has been computed.
#[derive(Clone, Copy)]
enum Dst {
    Reg(Register),
    /// Address already stored in L_T0.
    Mem,
    None,
}

pub struct FuncEmit<'a> {
    pub f: Function,
    cfg: &'a Cfg,
    /// Block start address -> dispatch index.
    index: HashMap<u32, u32>,
    order: Vec<u32>,
    live_out: HashMap<u32, u8>,
    /// Direct-call targets available in this module: entry -> wasm function index.
    func_index: &'a HashMap<u32, u32>,
    pending: Option<(FlagOp, u8)>,
    /// Flags some later instruction (or a successor) will read after the current one.
    needed: u8,
    ip: u32,
    next_ip: u32,
    /// Nesting depth of `if`/`block` constructs opened inside the current block's code.
    depth: u32,
    cur_block: usize,
    pub unimplemented: Vec<(u32, String)>,
}

impl<'a> FuncEmit<'a> {
    pub(crate) fn ip(&self) -> u32 {
        self.ip
    }

    pub fn new(cfg: &'a Cfg, func_index: &'a HashMap<u32, u32>) -> Self {
        let order: Vec<u32> = cfg.blocks.keys().copied().collect();
        let index = order.iter().enumerate().map(|(i, &a)| (a, i as u32)).collect();
        let live_out = liveness(cfg);
        FuncEmit {
            f: Function::new(LOCALS.to_vec()),
            cfg,
            index,
            order,
            live_out,
            func_index,
            pending: None,
            needed: 0,
            ip: 0,
            next_ip: 0,
            depth: 0,
            cur_block: 0,
            unimplemented: Vec::new(),
        }
    }

    pub(crate) fn i(&mut self, ins: W) {
        self.f.instruction(&ins);
    }

    pub(crate) fn c(&mut self, v: u32) {
        self.i(W::I32Const(v as i32));
    }

    pub(crate) fn get(&mut self, l: u32) {
        self.i(W::LocalGet(l));
    }

    pub(crate) fn set(&mut self, l: u32) {
        self.i(W::LocalSet(l));
    }

    pub(crate) fn tee(&mut self, l: u32) {
        self.i(W::LocalTee(l));
    }

    // ---- prologue / epilogue -------------------------------------------------

    fn load_state(&mut self) {
        for r in 0..8 {
            self.i(W::GlobalGet(Global::Eax as u32 + r));
            self.set(L_REG + r);
        }
        for fl in 0..6 {
            self.i(W::GlobalGet(Global::Cf as u32 + fl));
            self.set(L_FLAG + fl);
        }
    }

    fn spill_regs(&mut self) {
        for r in 0..8 {
            self.get(L_REG + r);
            self.i(W::GlobalSet(Global::Eax as u32 + r));
        }
    }

    fn spill_flags(&mut self) {
        self.materialize(flags::ALL);
        for fl in 0..6 {
            self.get(L_FLAG + fl);
            self.i(W::GlobalSet(Global::Cf as u32 + fl));
        }
    }

    fn reload_regs(&mut self) {
        for r in 0..8 {
            self.i(W::GlobalGet(Global::Eax as u32 + r));
            self.set(L_REG + r);
        }
    }

    fn reload_flags(&mut self) {
        for fl in 0..6 {
            self.i(W::GlobalGet(Global::Cf as u32 + fl));
            self.set(L_FLAG + fl);
        }
        self.pending = None;
    }

    pub(crate) fn spill_all(&mut self) {
        self.spill_regs();
        self.spill_flags();
    }

    pub(crate) fn reload_all(&mut self) {
        self.reload_regs();
        self.reload_flags();
    }

    fn call_rt(&mut self, f: RtFunc) {
        self.i(W::Call(f as u32));
    }

    pub(crate) fn trap(&mut self, kind: i32, arg: u32) {
        self.spill_all();
        self.i(W::I32Const(kind));
        self.c(arg);
        self.call_rt(RtFunc::Trap);
        self.reload_all();
    }

    pub(crate) fn unimplemented(&mut self, instr: &Instruction) {
        let mut text = String::new();
        iced_x86::FastFormatter::new().format(instr, &mut text);
        self.unimplemented.push((self.ip, text));
        self.trap(trap::UNIMPLEMENTED, self.ip);
    }

    // ---- registers -------------------------------------------------------

    fn reg_local(r: Register) -> u32 {
        L_REG + (r.full_register32() as u32 - Register::EAX as u32)
    }

    fn is_high8(r: Register) -> bool {
        matches!(r, Register::AH | Register::CH | Register::DH | Register::BH)
    }

    /// Pushes the register's value zero-extended to i32.
    pub(crate) fn read_reg(&mut self, r: Register) {
        let l = Self::reg_local(r);
        self.get(l);
        match r.size() {
            4 => {}
            2 => {
                self.c(0xffff);
                self.i(W::I32And);
            }
            1 => {
                if Self::is_high8(r) {
                    self.c(8);
                    self.i(W::I32ShrU);
                }
                self.c(0xff);
                self.i(W::I32And);
            }
            _ => unreachable!(),
        }
    }

    /// Stores local `src` into the register, merging partial widths.
    pub(crate) fn write_reg_from(&mut self, r: Register, src: u32) {
        let l = Self::reg_local(r);
        match r.size() {
            4 => {
                self.get(src);
                self.set(l);
            }
            2 => {
                self.get(l);
                self.c(0xffff_0000);
                self.i(W::I32And);
                self.get(src);
                self.c(0xffff);
                self.i(W::I32And);
                self.i(W::I32Or);
                self.set(l);
            }
            1 => {
                let (keep, shift) = if Self::is_high8(r) { (0xffff_00ff, 8) } else { (0xffff_ff00, 0) };
                self.get(l);
                self.c(keep);
                self.i(W::I32And);
                self.get(src);
                self.c(0xff);
                self.i(W::I32And);
                if shift != 0 {
                    self.c(shift);
                    self.i(W::I32Shl);
                }
                self.i(W::I32Or);
                self.set(l);
            }
            _ => unreachable!(),
        }
    }

    // ---- memory ----------------------------------------------------------

    /// Pushes the effective address of the memory operand.
    pub(crate) fn addr(&mut self, instr: &Instruction) {
        let mut terms = 0;
        if instr.memory_base() != Register::None {
            let b = instr.memory_base();
            if b.is_gpr32() {
                self.get(Self::reg_local(b));
            } else {
                self.read_reg(b);
            }
            terms += 1;
        }
        if instr.memory_index() != Register::None {
            let ix = instr.memory_index();
            if ix.is_gpr32() {
                self.get(Self::reg_local(ix));
            } else {
                self.read_reg(ix);
            }
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
        let disp = instr.memory_displacement32();
        if disp != 0 || terms == 0 {
            self.c(disp);
            if terms > 0 {
                self.i(W::I32Add);
            }
            terms += 1;
        }
        match instr.segment_prefix() {
            Register::FS => {
                self.i(W::GlobalGet(Global::FsBase as u32));
                self.i(W::I32Add);
            }
            Register::GS => {
                // GS is unused in 32-bit Windows; treat as zero base.
            }
            _ => {}
        }
        if instr.memory_base() == Register::ESP || instr.memory_base() == Register::EBP {
            // nothing special: the stack lives in guest memory like everything else
        }
        let _ = terms;
    }

    pub(crate) fn load(&mut self, w: u8) {
        self.i(match w {
            8 => W::I32Load8U(mem0()),
            16 => W::I32Load16U(mem0()),
            32 => W::I32Load(mem0()),
            _ => unreachable!(),
        });
    }

    pub(crate) fn store(&mut self, w: u8) {
        self.i(match w {
            8 => W::I32Store8(mem0()),
            16 => W::I32Store16(mem0()),
            32 => W::I32Store(mem0()),
            _ => unreachable!(),
        });
    }

    fn op_width(instr: &Instruction, n: u32) -> u8 {
        match instr.op_kind(n) {
            OpKind::Register => instr.op_register(n).size() as u8 * 8,
            OpKind::Memory => instr.memory_size().size() as u8 * 8,
            OpKind::Immediate8 | OpKind::Immediate8_2nd => 8,
            OpKind::Immediate16 | OpKind::Immediate8to16 => 16,
            _ => 32,
        }
    }

    /// Pushes operand `n` zero-extended. Memory operands use the address in L_T0 when
    /// `addr_in_t0`, otherwise compute it inline.
    pub(crate) fn read_op(&mut self, instr: &Instruction, n: u32, addr_in_t0: bool) {
        match instr.op_kind(n) {
            OpKind::Register => self.read_reg(instr.op_register(n)),
            OpKind::Memory => {
                if addr_in_t0 {
                    self.get(L_T0);
                } else {
                    self.addr(instr);
                }
                let w = (instr.memory_size().size() * 8) as u8;
                self.load(w);
            }
            OpKind::Immediate8 | OpKind::Immediate16 | OpKind::Immediate32 | OpKind::Immediate8to16 | OpKind::Immediate8to32
            | OpKind::Immediate8_2nd => {
                let w = Self::op_width(instr, 0).max(8);
                let v = instr.immediate(n) as u32 & mask(if instr.op_kind(n) == OpKind::Immediate8_2nd { 8 } else { w });
                self.c(v);
            }
            k => panic!("unsupported operand kind {k:?} at {:#x}", self.ip),
        }
    }

    /// For a destination operand: computes a memory address into L_T0 up front.
    fn prep_dst(&mut self, instr: &Instruction, n: u32) -> Dst {
        match instr.op_kind(n) {
            OpKind::Register => Dst::Reg(instr.op_register(n)),
            OpKind::Memory => {
                self.addr(instr);
                self.set(L_T0);
                Dst::Mem
            }
            _ => Dst::None,
        }
    }

    fn write_dst(&mut self, dst: Dst, w: u8, src: u32) {
        match dst {
            Dst::Reg(r) => self.write_reg_from(r, src),
            Dst::Mem => {
                self.get(L_T0);
                self.get(src);
                self.store(w);
            }
            Dst::None => {}
        }
    }

    fn read_dst(&mut self, dst: Dst, w: u8) {
        match dst {
            Dst::Reg(r) => self.read_reg(r),
            Dst::Mem => {
                self.get(L_T0);
                self.load(w);
            }
            Dst::None => self.c(0),
        }
    }

    // ---- flags -----------------------------------------------------------

    fn sext(&mut self, w: u8) {
        match w {
            8 => self.i(W::I32Extend8S),
            16 => self.i(W::I32Extend16S),
            _ => {}
        }
    }

    /// Pushes 0/1 for one lazily tracked flag.
    fn flag(&mut self, fl: u8) {
        let Some((op, w)) = self.pending else {
            self.get(L_FLAG + fl.trailing_zeros());
            return;
        };
        let w32 = w as u32;
        match fl {
            flags::ZF => {
                self.get(L_FR);
                self.i(W::I32Eqz);
            }
            flags::SF => {
                self.get(L_FR);
                self.c(w32 - 1);
                self.i(W::I32ShrU);
                self.c(1);
                self.i(W::I32And);
            }
            flags::PF => {
                self.get(L_FR);
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
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32Xor);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.c(4);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                _ => self.c(0),
            },
            flags::CF => match op {
                FlagOp::Add => {
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32LtU);
                }
                FlagOp::Adc => {
                    // carry-in ? res <= a : res < a
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32LeU);
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32LtU);
                    self.get(L_FC);
                    self.i(W::Select);
                }
                FlagOp::Sub => {
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32LtU);
                }
                FlagOp::Sbb => {
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32LeU);
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32LtU);
                    self.get(L_FC);
                    self.i(W::Select);
                }
                FlagOp::Logic => self.c(0),
                FlagOp::Inc | FlagOp::Dec => self.get(L_FLAG),
                FlagOp::Shl => {
                    // last bit shifted out = bit (w - n) of a, 0 when n > w
                    self.get(L_FA);
                    self.c(w32);
                    self.get(L_FB);
                    self.i(W::I32Sub);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.c(0);
                    self.get(L_FB);
                    self.c(w32);
                    self.i(W::I32LeU);
                    self.i(W::Select);
                }
                FlagOp::Shr | FlagOp::Sar => {
                    // bit (n - 1) of the (sign- or zero-extended) source
                    self.get(L_FA);
                    if op == FlagOp::Sar {
                        self.sext(w);
                    }
                    self.get(L_FB);
                    self.c(1);
                    self.i(W::I32Sub);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
            },
            flags::OF => match op {
                FlagOp::Add | FlagOp::Adc => {
                    self.get(L_FA);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.get(L_FB);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.i(W::I32And);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                FlagOp::Sub | FlagOp::Sbb => {
                    self.get(L_FA);
                    self.get(L_FB);
                    self.i(W::I32Xor);
                    self.get(L_FA);
                    self.get(L_FR);
                    self.i(W::I32Xor);
                    self.i(W::I32And);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                FlagOp::Logic | FlagOp::Sar => self.c(0),
                FlagOp::Inc => {
                    self.get(L_FR);
                    self.c(1 << (w32 - 1));
                    self.i(W::I32Eq);
                }
                FlagOp::Dec => {
                    self.get(L_FR);
                    self.c((1 << (w32 - 1)) - 1);
                    self.i(W::I32Eq);
                }
                FlagOp::Shl => {
                    // defined for n == 1: msb(res) ^ msb(a); otherwise 0
                    self.get(L_FR);
                    self.get(L_FA);
                    self.i(W::I32Xor);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.c(0);
                    self.get(L_FB);
                    self.c(1);
                    self.i(W::I32Eq);
                    self.i(W::Select);
                }
                FlagOp::Shr => {
                    self.get(L_FA);
                    self.c(w32 - 1);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                    self.c(0);
                    self.get(L_FB);
                    self.c(1);
                    self.i(W::I32Eq);
                    self.i(W::Select);
                }
            },
            _ => unreachable!(),
        }
    }

    /// Writes the given pending flags into their locals.
    pub(crate) fn materialize(&mut self, which: u8) {
        if self.pending.is_none() {
            return;
        }
        for fl in 0..6u32 {
            let bit = 1u8 << fl;
            if which & bit != 0 {
                self.flag(bit);
                self.set(L_FLAG + fl);
            }
        }
        if which & flags::ALL == flags::ALL {
            self.pending = None;
        } else {
            // Partially materialized: the rest stays pending, but a reader of a
            // materialized flag must now use the local. Simplest: materialize all.
            for fl in 0..6u32 {
                let bit = 1u8 << fl;
                if which & bit == 0 {
                    self.flag(bit);
                    self.set(L_FLAG + fl);
                }
            }
            self.pending = None;
        }
    }

    /// Records a lazily evaluated flag result. The operands must already be in
    /// L_FA / L_FB (/ L_FC) and the masked result in L_FR.
    fn set_pending(&mut self, op: FlagOp, w: u8) {
        self.pending = Some((op, w));
    }

    fn set_flag_const(&mut self, fl: u8, v: u32) {
        self.materialize(flags::ALL);
        self.c(v);
        self.set(L_FLAG + fl.trailing_zeros());
    }

    /// Pushes 0/1 for a condition code.
    fn cond(&mut self, cc: ConditionCode) {
        use ConditionCode as CC;
        if let Some((FlagOp::Sub, w)) = self.pending {
            // Compare fast paths straight from the operands.
            let unsigned = |me: &mut Self, op: W<'static>| { me.get(L_FA); me.get(L_FB); me.i(op); };
            let signed = |me: &mut Self, op: W<'static>| { me.get(L_FA); me.sext(w); me.get(L_FB); me.sext(w); me.i(op); };
            match cc {
                CC::b => return unsigned(self, W::I32LtU),
                CC::ae => return unsigned(self, W::I32GeU),
                CC::be => return unsigned(self, W::I32LeU),
                CC::a => return unsigned(self, W::I32GtU),
                CC::e => { self.get(L_FR); self.i(W::I32Eqz); return; }
                CC::ne => { self.get(L_FR); self.c(0); self.i(W::I32Ne); return; }
                CC::l => return signed(self, W::I32LtS),
                CC::ge => return signed(self, W::I32GeS),
                CC::le => return signed(self, W::I32LeS),
                CC::g => return signed(self, W::I32GtS),
                _ => {}
            }
        }
        match cc {
            CC::None => self.c(1),
            CC::o => self.flag(flags::OF),
            CC::no => { self.flag(flags::OF); self.i(W::I32Eqz); }
            CC::b => self.flag(flags::CF),
            CC::ae => { self.flag(flags::CF); self.i(W::I32Eqz); }
            CC::e => self.flag(flags::ZF),
            CC::ne => { self.flag(flags::ZF); self.i(W::I32Eqz); }
            CC::be => { self.flag(flags::CF); self.flag(flags::ZF); self.i(W::I32Or); }
            CC::a => { self.flag(flags::CF); self.flag(flags::ZF); self.i(W::I32Or); self.i(W::I32Eqz); }
            CC::s => self.flag(flags::SF),
            CC::ns => { self.flag(flags::SF); self.i(W::I32Eqz); }
            CC::p => self.flag(flags::PF),
            CC::np => { self.flag(flags::PF); self.i(W::I32Eqz); }
            CC::l => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Ne); }
            CC::ge => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Eq); }
            CC::le => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Ne); self.flag(flags::ZF); self.i(W::I32Or); }
            CC::g => { self.flag(flags::SF); self.flag(flags::OF); self.i(W::I32Ne); self.flag(flags::ZF); self.i(W::I32Or); self.i(W::I32Eqz); }
        }
    }

    // ---- control flow ----------------------------------------------------

    fn loop_depth(&self) -> u32 {
        (self.order.len() as u32 - 1 - self.cur_block as u32) + self.depth
    }

    /// Jumps to another block of this function.
    fn goto(&mut self, target: u32) {
        let idx = self.index[&target];
        self.c(idx);
        self.set(L_BLK);
        self.i(W::Br(self.loop_depth()));
    }

    fn push32_from(&mut self, src: u32) {
        self.get(L_REG + 4);
        self.c(4);
        self.i(W::I32Sub);
        self.tee(L_REG + 4);
        self.get(src);
        self.store(32);
    }

    fn push32_const(&mut self, v: u32) {
        self.get(L_REG + 4);
        self.c(4);
        self.i(W::I32Sub);
        self.tee(L_REG + 4);
        self.c(v);
        self.store(32);
    }

    /// Pops a dword into local `dst`.
    fn pop32_to(&mut self, dst: u32) {
        self.get(L_REG + 4);
        self.load(32);
        self.set(dst);
        self.get(L_REG + 4);
        self.c(4);
        self.i(W::I32Add);
        self.set(L_REG + 4);
    }

    /// Calls the guest function at a static address.
    fn call_static(&mut self, target: u32) {
        self.push32_const(self.next_ip);
        self.spill_all();
        if let Some(&idx) = self.func_index.get(&target) {
            self.i(W::Call(idx));
        } else {
            self.c(target);
            self.call_rt(RtFunc::Resolve);
            self.i(W::CallIndirect { type_index: types::FUNC, table_index: 0 });
        }
        self.after_call();
    }

    /// Calls the guest function whose address is in L_T1.
    fn call_dynamic(&mut self) {
        self.push32_const(self.next_ip);
        self.spill_all();
        self.get(L_T1);
        self.call_rt(RtFunc::Resolve);
        self.i(W::CallIndirect { type_index: types::FUNC, table_index: 0 });
        self.after_call();
    }

    fn after_call(&mut self) {
        self.reload_all();
        // The callee's `ret` left the continuation in eip; anything else means the
        // guest unwound past us, so keep returning until a frame matches.
        self.i(W::GlobalGet(Global::Eip as u32));
        self.c(self.next_ip);
        self.i(W::I32Ne);
        self.i(W::If(BlockType::Empty));
        self.i(W::Return);
        self.i(W::End);
    }

    fn ret(&mut self, extra: u32) {
        self.pop32_to(L_T0);
        if extra != 0 {
            self.get(L_REG + 4);
            self.c(extra);
            self.i(W::I32Add);
            self.set(L_REG + 4);
        }
        self.get(L_T0);
        self.i(W::GlobalSet(Global::Eip as u32));
        self.spill_all();
        self.i(W::Return);
    }

    /// Tail-jumps to the guest address in L_T1.
    fn tail_dynamic(&mut self) {
        self.spill_all();
        self.get(L_T1);
        self.call_rt(RtFunc::Resolve);
        self.i(W::ReturnCallIndirect { type_index: types::FUNC, table_index: 0 });
    }

    fn tail_static(&mut self, target: u32) {
        self.spill_all();
        if let Some(&idx) = self.func_index.get(&target) {
            self.i(W::ReturnCall(idx));
        } else {
            self.c(target);
            self.call_rt(RtFunc::Resolve);
            self.i(W::ReturnCallIndirect { type_index: types::FUNC, table_index: 0 });
        }
    }

    // ---- function body ---------------------------------------------------

    pub fn emit(&mut self) {
        self.load_state();
        let n = self.order.len() as u32;
        // block $fail; loop $top; block b_{n-1} ... block b_0; br_table; end b_0; code 0; end b_1; code 1; ...
        self.i(W::Block(BlockType::Empty));
        self.i(W::Loop(BlockType::Empty));
        for _ in 0..n {
            self.i(W::Block(BlockType::Empty));
        }
        self.get(L_BLK);
        let targets: Vec<u32> = (0..n).collect();
        self.i(W::BrTable(Cow::Owned(targets), n + 1));
        for k in 0..n as usize {
            self.i(W::End);
            self.cur_block = k;
            self.depth = 0;
            self.pending = None;
            let block = &self.cfg.blocks[&self.order[k]];
            self.emit_block(block);
        }
        self.i(W::End); // loop
        self.i(W::End); // fail
        self.i(W::Unreachable);
        self.i(W::End);
    }

    fn emit_block(&mut self, block: &Block) {
        let live_out = self.live_out[&block.start];
        // Flags each instruction must leave available for later readers.
        let mut needs = vec![0u8; block.instrs.len()];
        let mut need = live_out;
        for (k, instr) in block.instrs.iter().enumerate().rev() {
            needs[k] = need;
            let (def, used) = flags::effects(instr);
            need = (need & !def) | used;
        }
        let last = block.instrs.len().saturating_sub(1);
        for (k, instr) in block.instrs.iter().enumerate() {
            self.ip = instr.ip() as u32;
            self.next_ip = instr.next_ip() as u32;
            self.needed = needs[k];
            let is_term = k == last && !matches!(block.term, Term::Fall(_));
            if is_term {
                self.emit_terminator(instr, &block.term, live_out);
            } else {
                self.emit_instr(instr);
            }
        }
        if let Term::Fall(next) = block.term {
            self.materialize(live_out);
            if self.index[&next] != self.cur_block as u32 + 1 {
                self.goto(next);
            }
        }
    }

    fn emit_terminator(&mut self, instr: &Instruction, term: &Term, live_out: u8) {
        match term {
            Term::Fall(_) => unreachable!(),
            Term::Jump(target) => {
                self.materialize(live_out);
                if self.index[target] != self.cur_block as u32 + 1 {
                    self.goto(*target);
                }
            }
            Term::Cond { taken, next } => {
                match instr.mnemonic() {
                    Mnemonic::Jecxz => {
                        self.get(L_REG + 1);
                        self.i(W::I32Eqz);
                    }
                    Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne => {
                        self.get(L_REG + 1);
                        self.c(1);
                        self.i(W::I32Sub);
                        self.tee(L_REG + 1);
                        self.c(0);
                        self.i(W::I32Ne);
                        if instr.mnemonic() != Mnemonic::Loop {
                            self.flag(flags::ZF);
                            if instr.mnemonic() == Mnemonic::Loopne {
                                self.i(W::I32Eqz);
                            }
                            self.i(W::I32And);
                        }
                    }
                    _ => self.cond(instr.condition_code()),
                }
                self.set(L_T2);
                self.materialize(live_out);
                self.get(L_T2);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.goto(*taken);
                self.depth -= 1;
                self.i(W::End);
                if self.index[next] != self.cur_block as u32 + 1 {
                    self.goto(*next);
                }
            }
            Term::Table(targets) => {
                // jmp [idx*4 + table]: the index register selects the case.
                self.materialize(live_out);
                let ix = instr.memory_index();
                let cases: Vec<u32> = targets.iter().map(|t| self.index[t]).collect();
                let depth = self.loop_depth();
                // Each case: set blk and branch to the loop. Use a nested block ladder;
                // the index is pushed inside the innermost block (operands do not cross blocks).
                let n = cases.len() as u32;
                for _ in 0..n {
                    self.i(W::Block(BlockType::Empty));
                }
                self.read_reg(ix);
                self.i(W::BrTable(Cow::Owned((0..n).collect()), n - 1));
                for (k, &case) in cases.iter().enumerate() {
                    self.i(W::End);
                    self.c(case);
                    self.set(L_BLK);
                    let remaining = n - 1 - k as u32;
                    self.i(W::Br(depth + remaining));
                }
            }
            Term::TailCall(target) => self.tail_static(*target),
            Term::IndirectTail => {
                // jmp r/m32
                self.read_op(instr, 0, false);
                self.set(L_T1);
                self.tail_dynamic();
            }
            Term::Ret => {
                let extra = if instr.op_count() > 0 { instr.immediate16() as u32 } else { 0 };
                self.ret(extra);
            }
            Term::Trap => {
                let kind = match instr.mnemonic() {
                    Mnemonic::Int3 => trap::INT3,
                    Mnemonic::Int => trap::INT,
                    Mnemonic::Ud2 => trap::UD2,
                    Mnemonic::Hlt => trap::HLT,
                    _ => trap::UNIMPLEMENTED,
                };
                let arg = if instr.mnemonic() == Mnemonic::Int { instr.immediate8() as u32 } else { self.ip };
                self.trap(kind, arg);
                // Continue at the next instruction through the dispatcher.
                self.c(self.next_ip);
                self.i(W::GlobalSet(Global::Eip as u32));
                self.spill_all();
                self.i(W::Return);
            }
        }
    }

    // ---- instructions ----------------------------------------------------

    /// Emits a binary ALU op `dst = dst <op> src` with lazy flags.
    fn alu(&mut self, instr: &Instruction, op: FlagOp, wop: fn(&mut Self), writes: bool) {
        let w = Self::op_width(instr, 0);
        if matches!(op, FlagOp::Adc | FlagOp::Sbb) {
            // The carry must be read before the operand locals of the pending op are reused.
            self.flag(flags::CF);
            self.set(L_FC);
            self.pending = None;
        }
        let dst = self.prep_dst(instr, 0);
        self.read_dst(dst, w);
        self.set(L_FA);
        self.read_op(instr, 1, false);
        self.set(L_FB);
        self.get(L_FA);
        self.get(L_FB);
        wop(self);
        if matches!(op, FlagOp::Adc | FlagOp::Sbb) {
            self.get(L_FC);
            if op == FlagOp::Adc { self.i(W::I32Add) } else { self.i(W::I32Sub) }
        }
        if w != 32 {
            self.c(mask(w));
            self.i(W::I32And);
        }
        self.set(L_FR);
        if writes {
            self.write_dst(dst, w, L_FR);
        }
        self.set_pending(op, w);
    }

    fn emit_instr(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        match instr.mnemonic() {
            Nop | Wait | Pause | Lfence | Mfence | Sfence | Prefetcht0 | Prefetcht1 | Prefetcht2 | Prefetchnta | Endbr32 => {}
            Mov => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register()
                    || instr.op1_kind() == OpKind::Register && instr.op1_register().is_segment_register()
                {
                    return self.unimplemented(instr);
                }
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_op(instr, 1, false);
                self.set(L_T1);
                self.write_dst(dst, w, L_T1);
            }
            Movzx | Movsx => {
                let dst = self.prep_dst(instr, 0);
                let sw = Self::op_width(instr, 1);
                self.read_op(instr, 1, false);
                if instr.mnemonic() == Movsx {
                    self.sext(sw);
                }
                self.set(L_T1);
                self.write_dst(dst, Self::op_width(instr, 0), L_T1);
            }
            Lea => {
                self.addr(instr);
                self.set(L_T1);
                let r = instr.op0_register();
                self.write_reg_from(r, L_T1);
            }
            Push => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register() {
                    return self.unimplemented(instr);
                }
                let w = Self::op_width(instr, 0);
                if w == 16 {
                    self.read_op(instr, 0, false);
                    self.set(L_T1);
                    self.get(L_REG + 4);
                    self.c(2);
                    self.i(W::I32Sub);
                    self.tee(L_REG + 4);
                    self.get(L_T1);
                    self.store(16);
                } else {
                    // push esp pushes the value before the decrement: read first.
                    self.read_op(instr, 0, false);
                    self.set(L_T1);
                    self.push32_from(L_T1);
                }
            }
            Pop => {
                if instr.op0_kind() == OpKind::Register && instr.op0_register().is_segment_register() {
                    return self.unimplemented(instr);
                }
                let w = Self::op_width(instr, 0);
                if w == 16 {
                    self.get(L_REG + 4);
                    self.load(16);
                    self.set(L_T1);
                    self.get(L_REG + 4);
                    self.c(2);
                    self.i(W::I32Add);
                    self.set(L_REG + 4);
                } else {
                    self.pop32_to(L_T1);
                }
                // The address of a memory destination is computed after the pop (esp updated).
                let dst = self.prep_dst(instr, 0);
                self.write_dst(dst, w, L_T1);
            }
            Add => self.alu(instr, FlagOp::Add, |s| s.i(W::I32Add), true),
            Adc => self.alu(instr, FlagOp::Adc, |s| s.i(W::I32Add), true),
            Sub => self.alu(instr, FlagOp::Sub, |s| s.i(W::I32Sub), true),
            Sbb => self.alu(instr, FlagOp::Sbb, |s| s.i(W::I32Sub), true),
            Cmp => self.alu(instr, FlagOp::Sub, |s| s.i(W::I32Sub), false),
            And => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32And), true),
            Or => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32Or), true),
            Xor => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32Xor), true),
            Test => self.alu(instr, FlagOp::Logic, |s| s.i(W::I32And), false),
            Inc | Dec => {
                let w = Self::op_width(instr, 0);
                // CF survives: make sure it is in its local before the result replaces the pending op.
                self.materialize(flags::ALL);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.set(L_FA);
                self.c(1);
                self.set(L_FB);
                self.get(L_FA);
                self.c(1);
                if instr.mnemonic() == Inc { self.i(W::I32Add) } else { self.i(W::I32Sub) }
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.set_pending(if instr.mnemonic() == Inc { FlagOp::Inc } else { FlagOp::Dec }, w);
            }
            Neg => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.c(0);
                self.set(L_FA);
                self.read_dst(dst, w);
                self.set(L_FB);
                self.c(0);
                self.get(L_FB);
                self.i(W::I32Sub);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.set_pending(FlagOp::Sub, w);
            }
            Not => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.c(mask(w));
                self.i(W::I32Xor);
                self.set(L_T1);
                self.write_dst(dst, w, L_T1);
            }
            Shl | Sal | Shr | Sar => self.shift(instr),
            Rol | Ror | Rcl | Rcr => self.rotate(instr),
            Shld | Shrd => self.double_shift(instr),
            Imul => self.imul(instr),
            Mul => self.mul(instr),
            Div | Idiv => self.div(instr),
            Cbw => {
                self.read_reg(Register::AL);
                self.i(W::I32Extend8S);
                self.set(L_T1);
                self.write_reg_from(Register::AX, L_T1);
            }
            Cwde => {
                self.read_reg(Register::AX);
                self.i(W::I32Extend16S);
                self.set(L_T1);
                self.write_reg_from(Register::EAX, L_T1);
            }
            Cwd => {
                self.read_reg(Register::AX);
                self.i(W::I32Extend16S);
                self.c(31);
                self.i(W::I32ShrS);
                self.set(L_T1);
                self.write_reg_from(Register::DX, L_T1);
            }
            Cdq => {
                self.get(L_REG);
                self.c(31);
                self.i(W::I32ShrS);
                self.set(L_REG + 2);
            }
            Xchg => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.set(L_T1);
                self.read_op(instr, 1, false);
                self.set(L_T2);
                self.write_dst(dst, w, L_T2);
                let r = instr.op1_register();
                self.write_reg_from(r, L_T1);
            }
            Cmovo | Cmovno | Cmovb | Cmovae | Cmove | Cmovne | Cmovbe | Cmova | Cmovs | Cmovns | Cmovp | Cmovnp | Cmovl
            | Cmovge | Cmovle | Cmovg => {
                let w = Self::op_width(instr, 0);
                let r = instr.op0_register();
                self.read_op(instr, 1, false);
                self.read_reg(r);
                self.cond(instr.condition_code());
                self.i(W::Select);
                self.set(L_T1);
                self.write_reg_from(r, L_T1);
                let _ = w;
            }
            Seto | Setno | Setb | Setae | Sete | Setne | Setbe | Seta | Sets | Setns | Setp | Setnp | Setl | Setge | Setle
            | Setg => {
                let dst = self.prep_dst(instr, 0);
                self.cond(instr.condition_code());
                self.set(L_T1);
                self.write_dst(dst, 8, L_T1);
            }
            Call => match instr.op0_kind() {
                OpKind::NearBranch32 | OpKind::NearBranch16 => {
                    let target = instr.near_branch_target() as u32;
                    self.call_static(target);
                }
                OpKind::Register | OpKind::Memory => {
                    self.read_op(instr, 0, false);
                    self.set(L_T1);
                    self.call_dynamic();
                }
                _ => self.unimplemented(instr),
            },
            Leave => {
                self.get(L_REG + 5);
                self.set(L_REG + 4);
                self.pop32_to(L_REG + 5);
            }
            Enter => {
                let size = instr.immediate16() as u32;
                let level = instr.immediate8_2nd();
                if level != 0 {
                    return self.unimplemented(instr);
                }
                self.push32_from(L_REG + 5);
                self.get(L_REG + 4);
                self.set(L_REG + 5);
                self.get(L_REG + 4);
                self.c(size);
                self.i(W::I32Sub);
                self.set(L_REG + 4);
            }
            Bt | Bts | Btr | Btc => self.bit_test(instr),
            Bsf | Bsr => {
                let w = Self::op_width(instr, 0);
                let r = instr.op0_register();
                self.read_op(instr, 1, false);
                self.set(L_T1);
                self.materialize(flags::ALL);
                self.get(L_T1);
                self.i(W::I32Eqz);
                self.set(L_FLAG + 3);
                self.get(L_T1);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                if instr.mnemonic() == Bsf {
                    self.get(L_T1);
                    self.i(W::I32Ctz);
                } else {
                    self.c(31);
                    self.get(L_T1);
                    self.i(W::I32Clz);
                    self.i(W::I32Sub);
                }
                self.set(L_T2);
                self.write_reg_from(r, L_T2);
                self.depth -= 1;
                self.i(W::End);
                let _ = w;
            }
            Bswap => {
                let r = instr.op0_register();
                self.get(Self::reg_local(r));
                self.set(L_T1);
                // ((x << 24) | ((x & 0xff00) << 8) | ((x >> 8) & 0xff00) | (x >> 24))
                self.get(L_T1); self.c(24); self.i(W::I32Shl);
                self.get(L_T1); self.c(0xff00); self.i(W::I32And); self.c(8); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_T1); self.c(8); self.i(W::I32ShrU); self.c(0xff00); self.i(W::I32And); self.i(W::I32Or);
                self.get(L_T1); self.c(24); self.i(W::I32ShrU); self.i(W::I32Or);
                self.set(Self::reg_local(r));
            }
            Xadd => {
                let w = Self::op_width(instr, 0);
                let dst = self.prep_dst(instr, 0);
                self.read_dst(dst, w);
                self.set(L_FA);
                self.read_op(instr, 1, false);
                self.set(L_FB);
                self.get(L_FA);
                self.get(L_FB);
                self.i(W::I32Add);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                let r = instr.op1_register();
                self.write_reg_from(r, L_FA);
                self.write_dst(dst, w, L_FR);
                self.set_pending(FlagOp::Add, w);
            }
            Cmpxchg => {
                let w = Self::op_width(instr, 0);
                let acc = match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
                let dst = self.prep_dst(instr, 0);
                self.read_reg(acc);
                self.set(L_FA);
                self.read_dst(dst, w);
                self.set(L_FB);
                self.get(L_FA);
                self.get(L_FB);
                self.i(W::I32Sub);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.set_pending(FlagOp::Sub, w);
                self.get(L_FA);
                self.get(L_FB);
                self.i(W::I32Eq);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.read_op(instr, 1, false);
                self.set(L_T1);
                self.write_dst(dst, w, L_T1);
                self.i(W::Else);
                self.write_reg_from(acc, L_FB);
                self.depth -= 1;
                self.i(W::End);
            }
            Cmpxchg8b => {
                self.addr(instr);
                self.set(L_T0);
                self.get(L_T0);
                self.i(W::I64Load(mem0()));
                self.set(L_Q0);
                // edx:eax == m64 ?
                self.get(L_REG + 2);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Const(32));
                self.i(W::I64Shl);
                self.get(L_REG);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Or);
                self.get(L_Q0);
                self.i(W::I64Eq);
                self.set(L_T1);
                self.materialize(flags::ALL);
                self.get(L_T1);
                self.set(L_FLAG + 3);
                self.get(L_T1);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.get(L_T0);
                self.get(L_REG + 1);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Const(32));
                self.i(W::I64Shl);
                self.get(L_REG + 3);
                self.i(W::I64ExtendI32U);
                self.i(W::I64Or);
                self.i(W::I64Store(mem0()));
                self.i(W::Else);
                self.get(L_Q0);
                self.i(W::I32WrapI64);
                self.set(L_REG);
                self.get(L_Q0);
                self.i(W::I64Const(32));
                self.i(W::I64ShrU);
                self.i(W::I32WrapI64);
                self.set(L_REG + 2);
                self.depth -= 1;
                self.i(W::End);
            }
            Lahf => {
                self.materialize(flags::ALL);
                // AH = SF:ZF:0:AF:0:PF:1:CF
                self.get(L_FLAG + 4); self.c(7); self.i(W::I32Shl);
                self.get(L_FLAG + 3); self.c(6); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_FLAG + 2); self.c(4); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_FLAG + 1); self.c(2); self.i(W::I32Shl); self.i(W::I32Or);
                self.get(L_FLAG); self.i(W::I32Or);
                self.c(2); self.i(W::I32Or);
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
            Pushfd | Pushf => {
                self.eflags_to(L_T1);
                if instr.mnemonic() == Pushf {
                    self.get(L_REG + 4); self.c(2); self.i(W::I32Sub); self.tee(L_REG + 4); self.get(L_T1); self.store(16);
                } else {
                    self.push32_from(L_T1);
                }
            }
            Popfd | Popf => {
                if instr.mnemonic() == Popf {
                    self.get(L_REG + 4); self.load(16); self.set(L_T1);
                    self.get(L_REG + 4); self.c(2); self.i(W::I32Add); self.set(L_REG + 4);
                } else {
                    self.pop32_to(L_T1);
                }
                self.eflags_from(L_T1);
            }
            Pushad => {
                self.get(L_REG + 4);
                self.set(L_T1);
                for r in 0..8u32 {
                    if r == 4 { self.push32_from(L_T1); } else { self.push32_from(L_REG + r); }
                }
            }
            Popad => {
                for r in (0..8u32).rev() {
                    if r == 4 {
                        self.get(L_REG + 4); self.c(4); self.i(W::I32Add); self.set(L_REG + 4);
                    } else {
                        self.pop32_to(L_REG + r);
                    }
                }
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
            Cld => { self.c(0); self.i(W::GlobalSet(Global::Df as u32)); }
            Std => { self.c(1); self.i(W::GlobalSet(Global::Df as u32)); }
            Cpuid => {
                self.spill_all();
                self.call_rt(RtFunc::Cpuid);
                self.reload_all();
            }
            Rdtsc => {
                self.spill_all();
                self.call_rt(RtFunc::Rdtsc);
                self.reload_all();
            }
            Movsb | Movsw | Movsd if instr.op0_kind() == OpKind::MemoryESEDI => self.string_op(instr),
            Stosb | Stosw | Stosd | Lodsb | Lodsw | Lodsd | Scasb | Scasw | Scasd | Cmpsb | Cmpsw => self.string_op(instr),
            Cmpsd if instr.op0_kind() == OpKind::MemorySegESI => self.string_op(instr),
            Xlatb => {
                self.get(L_REG + 3);
                self.read_reg(Register::AL);
                self.i(W::I32Add);
                self.load(8);
                self.set(L_T1);
                self.write_reg_from(Register::AL, L_T1);
            }
            Int3 | Int | Ud2 | Hlt | Into | Bound => {
                // Only reached when not a block terminator (cannot happen); treat as trap.
                self.trap(trap::UNIMPLEMENTED, self.ip);
            }
            Femms | Emms => self.x87(instr),
            m if format!("{m:?}").starts_with('F') && m != Wait => self.x87(instr),
            _ => {
                if !self.sse(instr) {
                    self.unimplemented(instr);
                }
            }
        }
    }

    fn eflags_to(&mut self, dst: u32) {
        self.materialize(flags::ALL);
        self.c(0x202); // bit 1 always set, IF set
        self.get(L_FLAG); self.i(W::I32Or);
        self.get(L_FLAG + 1); self.c(2); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 2); self.c(4); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 3); self.c(6); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 4); self.c(7); self.i(W::I32Shl); self.i(W::I32Or);
        self.i(W::GlobalGet(Global::Df as u32)); self.c(10); self.i(W::I32Shl); self.i(W::I32Or);
        self.get(L_FLAG + 5); self.c(11); self.i(W::I32Shl); self.i(W::I32Or);
        self.set(dst);
    }

    fn eflags_from(&mut self, src: u32) {
        self.materialize(flags::ALL);
        for (bit, fl) in [(0u32, 0u32), (2, 1), (4, 2), (6, 3), (7, 4), (11, 5)] {
            self.get(src);
            self.c(bit);
            self.i(W::I32ShrU);
            self.c(1);
            self.i(W::I32And);
            self.set(L_FLAG + fl);
        }
        self.get(src);
        self.c(10);
        self.i(W::I32ShrU);
        self.c(1);
        self.i(W::I32And);
        self.i(W::GlobalSet(Global::Df as u32));
    }

    fn shift(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        let dst = self.prep_dst(instr, 0);
        let is_sar = instr.mnemonic() == Mnemonic::Sar;
        let is_shr = instr.mnemonic() == Mnemonic::Shr;
        let op = if is_sar { FlagOp::Sar } else if is_shr { FlagOp::Shr } else { FlagOp::Shl };
        let static_count = match instr.op1_kind() {
            OpKind::Immediate8 => Some(instr.immediate8() as u32 & 31),
            OpKind::Register => None,
            _ => Some(1),
        };
        // The pending flag op still owns L_FA/L_FB/L_FR until it is replaced or materialized,
        // so a count of zero must not touch them and a run-time count materializes first.
        match static_count {
            Some(0) => {}
            Some(n) => {
                self.read_dst(dst, w);
                self.set(L_FA);
                self.c(n);
                self.set(L_FB);
                self.get(L_FA);
                if is_sar { self.sext(w); }
                self.c(n);
                self.i(if is_sar { W::I32ShrS } else if is_shr { W::I32ShrU } else { W::I32Shl });
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.set_pending(op, w);
            }
            None => {
                // Count in CL: a zero count leaves flags untouched, so decide at run time.
                self.materialize(flags::ALL);
                self.read_dst(dst, w);
                self.set(L_FA);
                self.read_reg(Register::CL);
                self.c(31);
                self.i(W::I32And);
                self.tee(L_FB);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.get(L_FA);
                if is_sar { self.sext(w); }
                self.get(L_FB);
                self.i(if is_sar { W::I32ShrS } else if is_shr { W::I32ShrU } else { W::I32Shl });
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                self.pending = Some((op, w));
                self.materialize(flags::ALL);
                self.depth -= 1;
                self.i(W::End);
                self.pending = None;
            }
        }
    }

    fn rotate(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        let dst = self.prep_dst(instr, 0);
        let count_static = match instr.op1_kind() {
            OpKind::Immediate8 => Some(instr.immediate8() as u32 & 31),
            OpKind::Register => None,
            _ => Some(1),
        };
        self.materialize(flags::ALL);
        self.read_dst(dst, w);
        self.set(L_FA);
        match count_static {
            Some(n) => { self.c(n); }
            None => { self.read_reg(Register::CL); self.c(31); self.i(W::I32And); }
        }
        self.set(L_FB);
        self.get(L_FB);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        match instr.mnemonic() {
            Mnemonic::Rol | Mnemonic::Ror => {
                let left = instr.mnemonic() == Mnemonic::Rol;
                // count mod width
                self.get(L_FB);
                self.c(w as u32 - 1);
                self.i(W::I32And);
                self.set(L_T1);
                if w == 32 {
                    self.get(L_FA);
                    self.get(L_T1);
                    self.i(if left { W::I32Rotl } else { W::I32Rotr });
                } else {
                    // (a << n) | (a >> (w - n)) within width
                    self.get(L_FA);
                    self.get(L_T1);
                    self.i(if left { W::I32Shl } else { W::I32ShrU });
                    self.get(L_FA);
                    self.c(w as u32);
                    self.get(L_T1);
                    self.i(W::I32Sub);
                    self.c(31);
                    self.i(W::I32And);
                    self.i(if left { W::I32ShrU } else { W::I32Shl });
                    self.i(W::I32Or);
                    self.c(mask(w));
                    self.i(W::I32And);
                }
                self.set(L_FR);
                self.write_dst(dst, w, L_FR);
                // CF = bit rotated into the low (rol) or high (ror) position
                self.get(L_FR);
                if left { self.c(1); self.i(W::I32And); } else { self.c(w as u32 - 1); self.i(W::I32ShrU); }
                self.set(L_FLAG);
                // OF (count == 1): msb(res) ^ CF for rol; msb(res) ^ bit(w-2) of res for ror
                self.get(L_FR);
                self.c(w as u32 - 1);
                self.i(W::I32ShrU);
                self.c(1);
                self.i(W::I32And);
                if left {
                    self.get(L_FLAG);
                } else {
                    self.get(L_FR);
                    self.c(w as u32 - 2);
                    self.i(W::I32ShrU);
                    self.c(1);
                    self.i(W::I32And);
                }
                self.i(W::I32Xor);
                self.set(L_FLAG + 5);
            }
            _ => {
                // rcl/rcr: rotate through carry, count mod (w + 1), done bit by bit (rare instructions).
                let left = instr.mnemonic() == Mnemonic::Rcl;
                self.get(L_FB);
                self.c(w as u32 + 1);
                self.i(W::I32RemU);
                self.tee(L_T1);
                self.i(W::If(BlockType::Empty));
                self.depth += 1;
                self.get(L_FA);
                self.set(L_FR);
                self.i(W::Block(BlockType::Empty));
                self.i(W::Loop(BlockType::Empty));
                self.get(L_T1);
                self.i(W::I32Eqz);
                self.i(W::BrIf(1));
                if left {
                    // new_cf = msb(res); res = (res << 1 | cf) & mask
                    self.get(L_FR); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_T2);
                    self.get(L_FR); self.c(1); self.i(W::I32Shl); self.get(L_FLAG); self.i(W::I32Or); self.c(mask(w)); self.i(W::I32And); self.set(L_FR);
                } else {
                    self.get(L_FR); self.c(1); self.i(W::I32And); self.set(L_T2);
                    self.get(L_FR); self.c(1); self.i(W::I32ShrU); self.get(L_FLAG); self.c(w as u32 - 1); self.i(W::I32Shl); self.i(W::I32Or); self.set(L_FR);
                }
                self.get(L_T2);
                self.set(L_FLAG);
                self.get(L_T1); self.c(1); self.i(W::I32Sub); self.set(L_T1);
                self.i(W::Br(0));
                self.i(W::End);
                self.i(W::End);
                self.write_dst(dst, w, L_FR);
                // OF (count == 1): msb(res) ^ cf (rcl) / msb(res) ^ bit(w-2)(res) (rcr)
                self.get(L_FR); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And);
                if left { self.get(L_FLAG); } else { self.get(L_FR); self.c(w as u32 - 2); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); }
                self.i(W::I32Xor);
                self.set(L_FLAG + 5);
                self.depth -= 1;
                self.i(W::End);
            }
        }
        self.depth -= 1;
        self.i(W::End);
    }

    fn double_shift(&mut self, instr: &Instruction) {
        // shld/shrd dst, src, count: 32-bit (and 16-bit) forms.
        let w = Self::op_width(instr, 0);
        let left = instr.mnemonic() == Mnemonic::Shld;
        let dst = self.prep_dst(instr, 0);
        self.materialize(flags::ALL);
        self.read_dst(dst, w);
        self.set(L_FA);
        self.read_op(instr, 1, false);
        self.set(L_T1);
        match instr.op2_kind() {
            OpKind::Immediate8 => self.c(instr.immediate8() as u32 & 31),
            _ => { self.read_reg(Register::CL); self.c(31); self.i(W::I32And); }
        }
        self.set(L_FB);
        self.get(L_FB);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        // 64-bit concatenation makes both widths uniform: for shld, hi=dst lo=src; shrd: hi=src lo=dst.
        let (hi, lo) = if left { (L_FA, L_T1) } else { (L_T1, L_FA) };
        self.get(hi); self.i(W::I64ExtendI32U); self.i(W::I64Const(w as i64)); self.i(W::I64Shl);
        self.get(lo); self.i(W::I64ExtendI32U); self.i(W::I64Or);
        self.set(L_Q0);
        if left {
            self.get(L_Q0); self.get(L_FB); self.i(W::I64ExtendI32U); self.i(W::I64Shl);
            self.i(W::I64Const(w as i64)); self.i(W::I64ShrU); self.i(W::I32WrapI64);
        } else {
            self.get(L_Q0); self.get(L_FB); self.i(W::I64ExtendI32U); self.i(W::I64ShrU); self.i(W::I32WrapI64);
        }
        self.c(mask(w));
        self.i(W::I32And);
        self.set(L_FR);
        self.write_dst(dst, w, L_FR);
        // CF: last bit shifted out
        if left {
            self.get(L_FA); self.c(w as u32); self.get(L_FB); self.i(W::I32Sub); self.i(W::I32ShrU); self.c(1); self.i(W::I32And);
        } else {
            self.get(L_FA); self.get(L_FB); self.c(1); self.i(W::I32Sub); self.i(W::I32ShrU); self.c(1); self.i(W::I32And);
        }
        self.set(L_FLAG);
        // ZF/SF/PF from result, OF = msb changed (count == 1), AF undefined
        self.get(L_FR); self.i(W::I32Eqz); self.set(L_FLAG + 3);
        self.get(L_FR); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 4);
        self.get(L_FR); self.c(0xff); self.i(W::I32And); self.i(W::I32Popcnt); self.c(1); self.i(W::I32And); self.c(1); self.i(W::I32Xor); self.set(L_FLAG + 1);
        self.get(L_FR); self.get(L_FA); self.i(W::I32Xor); self.c(w as u32 - 1); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 5);
        self.depth -= 1;
        self.i(W::End);
    }

    fn imul(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        self.materialize(flags::ALL);
        match instr.op_count() {
            1 => {
                // edx:eax = eax * r/m (signed); CF=OF = result does not fit the low half
                self.read_op(instr, 0, false);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                let acc = match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
                self.read_reg(acc);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                self.i(W::I64Mul);
                self.set(L_Q0);
                self.wide_result(w, true);
            }
            2 | 3 => {
                let r = instr.op0_register();
                let src_idx = if instr.op_count() == 3 { 1 } else { 1 };
                self.read_op(instr, src_idx, false);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                if instr.op_count() == 3 {
                    self.read_op(instr, 2, false);
                } else {
                    self.read_reg(r);
                }
                self.sext(w);
                self.i(W::I64ExtendI32S);
                self.i(W::I64Mul);
                self.set(L_Q0);
                self.get(L_Q0);
                self.i(W::I32WrapI64);
                self.c(mask(w));
                self.i(W::I32And);
                self.set(L_T1);
                self.write_reg_from(r, L_T1);
                // CF = OF = (full result != sign-extended low half)
                self.get(L_Q0);
                self.get(L_T1);
                self.sext(w);
                self.i(W::I64ExtendI32S);
                self.i(W::I64Ne);
                self.tee(L_FLAG);
                self.set(L_FLAG + 5);
            }
            _ => unreachable!(),
        }
    }

    fn mul(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        self.materialize(flags::ALL);
        self.read_op(instr, 0, false);
        self.i(W::I64ExtendI32U);
        let acc = match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
        self.read_reg(acc);
        self.i(W::I64ExtendI32U);
        self.i(W::I64Mul);
        self.set(L_Q0);
        self.wide_result(w, false);
    }

    /// Writes the 2w-bit product in L_Q0 to its destination and sets CF/OF.
    fn wide_result(&mut self, w: u8, signed: bool) {
        match w {
            8 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.c(0xffff); self.i(W::I32And); self.set(L_T1);
                self.write_reg_from(Register::AX, L_T1);
            }
            16 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.c(0xffff); self.i(W::I32And); self.set(L_T1);
                self.write_reg_from(Register::AX, L_T1);
                self.get(L_Q0); self.i(W::I64Const(16)); self.i(W::I64ShrU); self.i(W::I32WrapI64); self.c(0xffff); self.i(W::I32And); self.set(L_T1);
                self.write_reg_from(Register::DX, L_T1);
            }
            _ => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_REG);
                self.get(L_Q0); self.i(W::I64Const(32)); self.i(W::I64ShrU); self.i(W::I32WrapI64); self.set(L_REG + 2);
            }
        }
        // CF = OF = upper half is not the sign/zero extension of the lower half
        self.get(L_Q0);
        self.get(L_Q0);
        self.i(W::I32WrapI64);
        self.c(mask(w));
        self.i(W::I32And);
        if signed { self.sext(w); self.i(W::I64ExtendI32S); } else { self.i(W::I64ExtendI32U); }
        self.i(W::I64Ne);
        self.tee(L_FLAG);
        self.set(L_FLAG + 5);
    }

    fn div(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        let signed = instr.mnemonic() == Mnemonic::Idiv;
        self.materialize(flags::ALL);
        // dividend in L_Q0, divisor in L_Q1
        match w {
            8 => { self.read_reg(Register::AX); if signed { self.sext(16); self.i(W::I64ExtendI32S) } else { self.i(W::I64ExtendI32U) } }
            16 => {
                self.read_reg(Register::DX); self.i(W::I64ExtendI32U); self.i(W::I64Const(16)); self.i(W::I64Shl);
                self.read_reg(Register::AX); self.i(W::I64ExtendI32U); self.i(W::I64Or);
                if signed { self.i(W::I64Const(32)); self.i(W::I64Shl); self.i(W::I64Const(32)); self.i(W::I64ShrS); }
            }
            _ => {
                self.get(L_REG + 2); self.i(W::I64ExtendI32U); self.i(W::I64Const(32)); self.i(W::I64Shl);
                self.get(L_REG); self.i(W::I64ExtendI32U); self.i(W::I64Or);
            }
        }
        self.set(L_Q0);
        self.read_op(instr, 0, false);
        if signed { self.sext(w); self.i(W::I64ExtendI32S) } else { self.i(W::I64ExtendI32U) }
        self.set(L_Q1);
        // #DE on zero divisor or quotient overflow
        self.get(L_Q1);
        self.i(W::I64Eqz);
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        self.trap(trap::DIVIDE_ERROR, self.ip);
        self.depth -= 1;
        self.i(W::End);
        self.get(L_Q0);
        self.get(L_Q1);
        self.i(if signed { W::I64DivS } else { W::I64DivU });
        self.set(L_Q0); // quotient (temporarily reuse: remainder recomputed below)
        // remainder = dividend - quotient * divisor: recompute dividend
        match w {
            8 => { self.read_reg(Register::AX); if signed { self.sext(16); self.i(W::I64ExtendI32S) } else { self.i(W::I64ExtendI32U) } }
            16 => {
                self.read_reg(Register::DX); self.i(W::I64ExtendI32U); self.i(W::I64Const(16)); self.i(W::I64Shl);
                self.read_reg(Register::AX); self.i(W::I64ExtendI32U); self.i(W::I64Or);
                if signed { self.i(W::I64Const(32)); self.i(W::I64Shl); self.i(W::I64Const(32)); self.i(W::I64ShrS); }
            }
            _ => {
                self.get(L_REG + 2); self.i(W::I64ExtendI32U); self.i(W::I64Const(32)); self.i(W::I64Shl);
                self.get(L_REG); self.i(W::I64ExtendI32U); self.i(W::I64Or);
            }
        }
        self.get(L_Q0);
        self.get(L_Q1);
        self.i(W::I64Mul);
        self.i(W::I64Sub);
        self.set(L_Q1); // remainder
        // overflow check: quotient must fit in w bits (signed or unsigned)
        self.get(L_Q0);
        if signed {
            self.i(W::I64Const(-(1i64 << (w - 1))));
            self.i(W::I64LtS);
            self.get(L_Q0);
            self.i(W::I64Const((1i64 << (w - 1)) - 1));
            self.i(W::I64GtS);
            self.i(W::I32Or);
        } else {
            self.i(W::I64Const(mask(w) as i64));
            self.i(W::I64GtU);
        }
        self.i(W::If(BlockType::Empty));
        self.depth += 1;
        self.trap(trap::DIVIDE_ERROR, self.ip);
        self.depth -= 1;
        self.i(W::End);
        match w {
            8 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::AL, L_T1);
                self.get(L_Q1); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::AH, L_T1);
            }
            16 => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::AX, L_T1);
                self.get(L_Q1); self.i(W::I32WrapI64); self.set(L_T1); self.write_reg_from(Register::DX, L_T1);
            }
            _ => {
                self.get(L_Q0); self.i(W::I32WrapI64); self.set(L_REG);
                self.get(L_Q1); self.i(W::I32WrapI64); self.set(L_REG + 2);
            }
        }
    }

    fn bit_test(&mut self, instr: &Instruction) {
        let w = Self::op_width(instr, 0);
        self.materialize(flags::ALL);
        // bit offset
        match instr.op1_kind() {
            OpKind::Immediate8 => self.c(instr.immediate8() as u32),
            _ => { self.read_op(instr, 1, false); self.sext(w); }
        }
        self.set(L_T1);
        let dst = match instr.op0_kind() {
            OpKind::Memory if instr.op1_kind() == OpKind::Register => {
                // register bit offsets address memory beyond the operand: addr += (off >> log2(w)) * bytes
                self.addr(instr);
                self.get(L_T1);
                self.c((w as u32).trailing_zeros());
                self.i(W::I32ShrS);
                self.c((w as u32 / 8).trailing_zeros());
                self.i(W::I32Shl);
                self.i(W::I32Add);
                self.set(L_T0);
                Dst::Mem
            }
            _ => self.prep_dst(instr, 0),
        };
        self.get(L_T1);
        self.c(w as u32 - 1);
        self.i(W::I32And);
        self.set(L_T1);
        self.read_dst(dst, w);
        self.set(L_T2);
        self.get(L_T2);
        self.get(L_T1);
        self.i(W::I32ShrU);
        self.c(1);
        self.i(W::I32And);
        self.set(L_FLAG);
        let modify: Option<fn(&mut Self)> = match instr.mnemonic() {
            Mnemonic::Bts => Some(|s| s.i(W::I32Or)),
            Mnemonic::Btr => Some(|s| { s.c(0xffff_ffff); s.i(W::I32Xor); s.i(W::I32And); }),
            Mnemonic::Btc => Some(|s| s.i(W::I32Xor)),
            _ => None,
        };
        if let Some(m) = modify {
            self.get(L_T2);
            self.c(1);
            self.get(L_T1);
            self.i(W::I32Shl);
            m(self);
            self.c(mask(w));
            self.i(W::I32And);
            self.set(L_T2);
            self.write_dst(dst, w, L_T2);
        }
    }

    fn string_op(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        let m = instr.mnemonic();
        let w: u8 = match m { Movsb | Stosb | Lodsb | Scasb | Cmpsb => 8, Movsw | Stosw | Lodsw | Scasw | Cmpsw => 16, _ => 32 };
        let step = (w / 8) as u32;
        let rep = instr.has_rep_prefix() || instr.has_repe_prefix() || instr.has_repne_prefix();
        let compare = matches!(m, Scasb | Scasw | Scasd | Cmpsb | Cmpsw | Cmpsd);
        if compare { self.materialize(flags::ALL); }
        // delta = df ? -step : step  -> L_T3   (select keeps the first operand when the condition is set)
        self.c(step.wrapping_neg());
        self.c(step);
        self.i(W::GlobalGet(Global::Df as u32));
        self.i(W::Select);
        self.set(L_T3);
        if rep {
            self.i(W::Block(BlockType::Empty));
            self.i(W::Loop(BlockType::Empty));
            self.depth += 2;
            self.get(L_REG + 1);
            self.i(W::I32Eqz);
            self.i(W::BrIf(1));
        }
        match m {
            Movsb | Movsw | Movsd => {
                self.get(L_REG + 7);
                self.get(L_REG + 6);
                self.load(w);
                self.store(w);
                self.get(L_REG + 6); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 6);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
            }
            Stosb | Stosw | Stosd => {
                self.get(L_REG + 7);
                self.read_reg(match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX });
                self.store(w);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
            }
            Lodsb | Lodsw | Lodsd => {
                self.get(L_REG + 6);
                self.load(w);
                self.set(L_T1);
                self.write_reg_from(match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX }, L_T1);
                self.get(L_REG + 6); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 6);
            }
            Scasb | Scasw | Scasd => {
                self.read_reg(match w { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX });
                self.set(L_FA);
                self.get(L_REG + 7);
                self.load(w);
                self.set(L_FB);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
                self.cmp_flags(w);
            }
            Cmpsb | Cmpsw | Cmpsd => {
                self.get(L_REG + 6);
                self.load(w);
                self.set(L_FA);
                self.get(L_REG + 7);
                self.load(w);
                self.set(L_FB);
                self.get(L_REG + 6); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 6);
                self.get(L_REG + 7); self.get(L_T3); self.i(W::I32Add); self.set(L_REG + 7);
                self.cmp_flags(w);
            }
            _ => unreachable!(),
        }
        if rep {
            self.get(L_REG + 1); self.c(1); self.i(W::I32Sub); self.set(L_REG + 1);
            if compare {
                // repe stops when ZF == 0, repne when ZF == 1
                self.get(L_FLAG + 3);
                if instr.has_repne_prefix() { self.i(W::BrIf(1)); } else { self.i(W::I32Eqz); self.i(W::BrIf(1)); }
            }
            self.i(W::Br(0));
            self.depth -= 2;
            self.i(W::End);
            self.i(W::End);
        }
    }

    /// Materialized compare flags from L_FA - L_FB (used by string compares).
    fn cmp_flags(&mut self, w: u8) {
        self.get(L_FA);
        self.get(L_FB);
        self.i(W::I32Sub);
        self.c(mask(w));
        self.i(W::I32And);
        self.set(L_FR);
        self.pending = Some((FlagOp::Sub, w));
        self.materialize(flags::ALL);
    }

    fn x87(&mut self, instr: &Instruction) {
        use Mnemonic::*;
        // a = memory address or ST index of op0; b packs op1's ST index, the memory operand
        // size and which operands are ST registers (see fpu::args).
        let has_mem = (0..instr.op_count()).any(|n| instr.op_kind(n) == OpKind::Memory);
        let op0_st = instr.op_count() > 0 && instr.op0_kind() == OpKind::Register && instr.op0_register().is_st();
        let op1_st = instr.op_count() > 1 && instr.op1_kind() == OpKind::Register && instr.op1_register().is_st();
        if has_mem {
            self.addr(instr);
        } else if op0_st {
            self.c(instr.op0_register().number() as u32);
        } else {
            self.c(0);
        }
        self.set(L_T1);
        let mut b: u32 = 0;
        if op1_st {
            b |= instr.op1_register().number() as u32;
        }
        if has_mem {
            b |= (instr.memory_size() as u32) << 8;
        }
        if op0_st {
            b |= 1 << 16;
        }
        if op1_st {
            b |= 1 << 17;
        }
        let is_fcmov = matches!(instr.mnemonic(), Fcmovb | Fcmove | Fcmovbe | Fcmovu | Fcmovnb | Fcmovne | Fcmovnbe | Fcmovnu);
        if is_fcmov {
            // The condition lives in the integer flags: evaluate it here and pass it in bit 24.
            let cc = match instr.mnemonic() {
                Fcmovb => ConditionCode::b,
                Fcmove => ConditionCode::e,
                Fcmovbe => ConditionCode::be,
                Fcmovu => ConditionCode::p,
                Fcmovnb => ConditionCode::ae,
                Fcmovne => ConditionCode::ne,
                Fcmovnbe => ConditionCode::a,
                _ => ConditionCode::np,
            };
            self.c(b);
            self.cond(cc);
            self.c(24);
            self.i(W::I32Shl);
            self.i(W::I32Or);
        } else {
            self.c(b);
        }
        self.set(L_T2);
        let writes_flags = matches!(instr.mnemonic(), Fcomi | Fcomip | Fucomi | Fucomip);
        let writes_ax = instr.mnemonic() == Fnstsw && !has_mem;
        if writes_flags {
            self.materialize(flags::ALL);
        }
        self.i(W::I32Const(instr.code() as i32));
        self.get(L_T1);
        self.get(L_T2);
        self.c(self.ip);
        self.c(self.cfg.fop.get(&self.ip).copied().unwrap_or(0) as u32);
        self.call_rt(RtFunc::Fpu);
        if writes_ax {
            self.set(L_T1);
            self.write_reg_from(Register::AX, L_T1);
        } else if writes_flags {
            self.set(L_T1);
            // return value uses EFLAGS bit positions: CF=0, PF=2, ZF=6
            self.get(L_T1); self.c(1); self.i(W::I32And); self.set(L_FLAG);
            self.get(L_T1); self.c(2); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 1);
            self.get(L_T1); self.c(6); self.i(W::I32ShrU); self.c(1); self.i(W::I32And); self.set(L_FLAG + 3);
            self.c(0); self.set(L_FLAG + 2);
            self.c(0); self.set(L_FLAG + 4);
            self.c(0); self.set(L_FLAG + 5);
        } else {
            self.i(W::Drop);
        }
    }
}

/// Flags live into each block (and out, via successors). Conservatively assumes
/// returns and tail/indirect jumps need every flag.
fn liveness(cfg: &Cfg) -> HashMap<u32, u8> {
    let mut usedef: HashMap<u32, (u8, u8)> = HashMap::new();
    for (&start, b) in &cfg.blocks {
        let (mut used, mut def) = (0u8, 0u8);
        for i in &b.instrs {
            let (d, u) = flags::effects(i);
            used |= u & !def;
            def |= d;
        }
        usedef.insert(start, (used, def));
    }
    let mut live_in: HashMap<u32, u8> = cfg.blocks.keys().map(|&k| (k, 0)).collect();
    let mut live_out: HashMap<u32, u8> = cfg.blocks.keys().map(|&k| (k, 0)).collect();
    loop {
        let mut changed = false;
        for (&start, b) in cfg.blocks.iter().rev() {
            let out = match b.term {
                Term::Ret | Term::TailCall(_) | Term::IndirectTail | Term::Trap => flags::ALL,
                _ => b.successors().iter().map(|s| live_in[s]).fold(0, |a, x| a | x),
            };
            let (used, def) = usedef[&start];
            let inn = used | (out & !def);
            if live_out[&start] != out || live_in[&start] != inn {
                live_out.insert(start, out);
                live_in.insert(start, inn);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    live_out
}

pub fn global_index(g: Global) -> u32 {
    g as u32
}

pub fn rt_index(f: RtFunc) -> u32 {
    f as u32
}

pub const _ASSERT_GLOBALS: () = assert!(abi::GLOBALS.len() == 26);
